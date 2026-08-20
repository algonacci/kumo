//! Polls for due scheduled tasks and runs each one through the same agent loop as an ordinary
//! Telegram message, delivering the result to the chat that scheduled it.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use teloxide::prelude::*;
use tokio::sync::{Mutex, RwLock};

use crate::{
    AppState, PendingApprovals, PendingQuestions, logging, prepare_history,
    provider::Message as ProviderMessage,
    run_agent, send_formatted, send_mcp_images,
    storage::{Database, ScheduledTask, StaleScheduledTask, StaleTaskOutcome},
};

/// How often to check for due tasks. Coarser than a typical cron minimum, but scheduled tasks are
/// a personal-assistant feature, not a precision timer, so this is a deliberate simplicity trade.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// A task more than this far past its `run_at` (e.g. Kumo was offline) is expired instead of run,
/// since a reminder this late is unlikely to still be useful.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// Runs until the process exits; intended to be spawned once alongside the Telegram dispatcher.
/// Shares `turn_lock` with `handle_message` so a scheduled task and an incoming message never run
/// their agent loops concurrently and contend over the same approval slot.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    bot: Bot,
    state: Arc<RwLock<AppState>>,
    approvals: PendingApprovals,
    questions: PendingQuestions,
    database: Arc<Mutex<Database>>,
    turn_lock: Arc<Mutex<()>>,
) {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    loop {
        interval.tick().await;
        if let Err(error) =
            run_due_tasks(&bot, &state, &approvals, &questions, &database, &turn_lock).await
        {
            logging::error("scheduler", "poll failed", &error);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_due_tasks(
    bot: &Bot,
    state: &Arc<RwLock<AppState>>,
    approvals: &PendingApprovals,
    questions: &PendingQuestions,
    database: &Arc<Mutex<Database>>,
    turn_lock: &Arc<Mutex<()>>,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();

    let finished_jobs = database.lock().await.unnotified_command_jobs()?;
    for job in finished_jobs {
        let chat_id = ChatId(job.telegram_chat_id);
        let message = format!(
            "Background command finished:\n{}",
            crate::tools::format_job(&job)
        );
        let mut delivered = true;
        for chunk in crate::message_chunks(&message, 4000) {
            if let Err(error) = bot.send_message(chat_id, chunk).await {
                delivered = false;
                eprintln!(
                    "Could not notify chat {chat_id} about job {}: {error:#}",
                    &job.id[..8]
                );
                break;
            }
        }
        if delivered {
            database.lock().await.mark_command_job_notified(&job.id)?;
        }
    }

    // Occurrences that missed their window by too long (e.g. Kumo was offline) are skipped rather
    // than run late and silently; tell the owning chat why nothing happened at the scheduled time,
    // and — for a recurring task, which is not over — when it will happen next.
    let stale = database
        .lock()
        .await
        .expire_stale_scheduled_tasks(now, STALE_AFTER.as_secs() as i64)?;
    if !stale.is_empty() {
        let timezone = state.read().await.config.timezone();
        for entry in stale {
            let chat_id = ChatId(entry.task.telegram_chat_id);
            let reason = match entry.outcome {
                StaleTaskOutcome::Expired => "expired",
                StaleTaskOutcome::Rescheduled { .. } => "rescheduled",
            };
            logging::info(
                "scheduler",
                format!(
                    "task={} chat_id={} status=stale due_at={} outcome={reason}",
                    &entry.task.id[..8],
                    chat_id,
                    entry.task.run_at
                ),
            );
            let notice = stale_notice(&entry, timezone);
            if let Err(error) = bot.send_message(chat_id, notice).await {
                logging::warn(
                    "scheduler",
                    format!(
                        "task={} chat_id={} notification=failed reason={reason} error={error:#}",
                        &entry.task.id[..8],
                        chat_id
                    ),
                );
            }
        }
    }

    let due = database.lock().await.claim_due_scheduled_tasks(now)?;
    for task in due {
        let _turn_guard = turn_lock.lock().await;
        let chat_id = ChatId(task.telegram_chat_id);
        logging::info(
            "scheduler",
            format!(
                "task={} chat_id={} status=started due_at={}",
                &task.id[..8],
                chat_id,
                task.run_at
            ),
        );

        let outcome = run_scheduled_task(
            bot,
            chat_id,
            state,
            approvals,
            questions,
            database,
            &task.prompt,
            recurrence_note(&task),
        )
        .await;
        let status = match &outcome {
            Ok(()) => "completed",
            Err(error) => {
                logging::error(
                    "scheduler",
                    format!("task={} status=failed", &task.id[..8]),
                    error,
                );
                let repeat_note = if task.repeat_interval_seconds.is_some() {
                    " This recurring reminder will not be retried automatically; reschedule it if needed."
                } else {
                    ""
                };
                let notice = format!(
                    "\u{26a0}\u{fe0f} A scheduled task failed to run: \"{}\"\nError: {error:#}{repeat_note}",
                    task.prompt
                );
                if let Err(send_error) = bot.send_message(chat_id, notice).await {
                    logging::warn(
                        "scheduler",
                        format!(
                            "task={} chat_id={} notification=failed error={send_error:#}",
                            &task.id[..8],
                            chat_id
                        ),
                    );
                }
                "failed"
            }
        };
        // For a successful, recurring task this reschedules it (run_at advanced by its interval)
        // instead of actually marking it completed — see Database::complete_scheduled_task.
        database
            .lock()
            .await
            .complete_scheduled_task(&task.id, status)?;
    }
    Ok(())
}

/// The notice sent when a task's occurrence was missed by more than `STALE_AFTER`.
///
/// The two cases have to read differently. A one-shot task is over, and saying only "skipped"
/// leaves that unsaid. A recurring task is *not* over — it used to be, which is the bug this
/// wording exists to close — so its notice names the occurrences that were dropped, says when the
/// next one is, and carries the same cancel line a delivery would, because a reminder the owner no
/// longer wants is exactly what a "you missed some of these" message brings to mind.
fn stale_notice(entry: &StaleScheduledTask, timezone: chrono_tz::Tz) -> String {
    match entry.outcome {
        StaleTaskOutcome::Expired => format!(
            "\u{26a0}\u{fe0f} A scheduled task missed its time by more than an hour and was skipped: \"{}\"\nIt was a one-off, so it will not run at all.",
            entry.task.prompt
        ),
        StaleTaskOutcome::Rescheduled {
            skipped,
            next_run_at,
        } => {
            let occurrences = if skipped == 1 {
                "1 occurrence was skipped".to_owned()
            } else {
                format!("{skipped} occurrences were skipped")
            };
            let mut notice = format!(
                "\u{26a0}\u{fe0f} A recurring task missed its time by more than an hour, so {occurrences}: \"{}\"\nNext run: {}.",
                entry.task.prompt,
                local_time(next_run_at, timezone)
            );
            if let Some(note) = recurrence_note(&entry.task) {
                notice.push('\n');
                notice.push_str(&note);
            }
            notice
        }
    }
}

/// A unix timestamp rendered in the owner's configured timezone, the way `/reminders` renders one.
fn local_time(timestamp: i64, timezone: chrono_tz::Tz) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| {
            value
                .with_timezone(&timezone)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string()
        })
        .unwrap_or_else(|| "an unknown time".to_owned())
}

/// The line appended to a recurring reminder telling the reader how to stop it. A reminder that
/// turns out to be unwanted is discovered *by being delivered*, so the way out belongs in the
/// message doing the interrupting — not in a command the reader has to already know about. One-shot
/// tasks get nothing: there is nothing to stop.
fn recurrence_note(task: &ScheduledTask) -> Option<String> {
    let interval = task.repeat_interval_seconds?;
    let every = match interval {
        seconds if seconds % 86_400 == 0 => format!("{} day(s)", seconds / 86_400),
        seconds if seconds % 3_600 == 0 => format!("{} hour(s)", seconds / 3_600),
        seconds if seconds % 60 == 0 => format!("{} minute(s)", seconds / 60),
        seconds => format!("{seconds} second(s)"),
    };
    Some(format!(
        "Repeats every {every}. To stop it, send: /reminders cancel {}",
        &task.id[..8]
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_scheduled_task(
    bot: &Bot,
    chat_id: ChatId,
    state: &Arc<RwLock<AppState>>,
    approvals: &PendingApprovals,
    questions: &PendingQuestions,
    database: &Arc<Mutex<Database>>,
    prompt: &str,
    recurrence_note: Option<String>,
) -> Result<()> {
    let history = prepare_history(state, database, chat_id.0).await?;
    let turn = run_agent(
        bot,
        chat_id,
        state,
        approvals,
        questions,
        database,
        history,
        ProviderMessage::user(prompt),
    )
    .await?;

    bot.send_message(chat_id, "\u{23f0} Scheduled task:")
        .await?;
    for chunk in crate::message_chunks(&turn.answer, 4000) {
        send_formatted(bot, chat_id, &chunk).await?;
    }
    send_mcp_images(bot, chat_id, &turn.images).await?;
    if let Some(note) = recurrence_note {
        bot.send_message(chat_id, note).await?;
    }
    database.lock().await.save_turn(
        chat_id.0,
        &turn.model,
        &turn.record,
        &turn.usage,
        &turn.finish_reason,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(repeat_interval_seconds: Option<i64>) -> ScheduledTask {
        ScheduledTask {
            id: "8be7f7aa-1111-2222-3333-444444444444".to_owned(),
            telegram_chat_id: 42,
            prompt: "ping".to_owned(),
            run_at: 0,
            repeat_interval_seconds,
        }
    }

    #[test]
    fn a_one_shot_reminder_carries_no_note() {
        assert!(recurrence_note(&task(None)).is_none());
    }

    #[test]
    fn a_recurring_reminder_says_how_to_stop_itself() {
        let note = recurrence_note(&task(Some(86_400))).unwrap();

        assert!(note.contains("Repeats every 1 day(s)"), "{note}");
        // The id has to be the short form /reminders cancel accepts.
        assert!(note.contains("/reminders cancel 8be7f7aa"), "{note}");
    }

    #[test]
    fn a_stale_one_shot_notice_says_the_task_is_over() {
        let notice = stale_notice(
            &StaleScheduledTask {
                task: task(None),
                outcome: StaleTaskOutcome::Expired,
            },
            chrono_tz::UTC,
        );

        assert!(notice.contains("was skipped"), "{notice}");
        assert!(notice.contains("will not run at all"), "{notice}");
        assert!(
            !notice.contains("/reminders cancel"),
            "there is nothing left to cancel: {notice}"
        );
    }

    #[test]
    fn a_stale_recurring_notice_says_when_it_will_next_run() {
        let notice = stale_notice(
            &StaleScheduledTask {
                task: task(Some(86_400)),
                outcome: StaleTaskOutcome::Rescheduled {
                    skipped: 2,
                    next_run_at: 1_700_000_000,
                },
            },
            chrono_tz::UTC,
        );

        assert!(notice.contains("2 occurrences were skipped"), "{notice}");
        assert!(
            notice.contains("Next run: 2023-11-14 22:13 UTC"),
            "{notice}"
        );
        // The escape hatch travels with the interruption, as it does on a delivery.
        assert!(notice.contains("/reminders cancel 8be7f7aa"), "{notice}");
        assert!(
            !notice.contains("will not run"),
            "a recurring task is not over: {notice}"
        );
    }

    #[test]
    fn a_single_skipped_occurrence_reads_as_one() {
        let notice = stale_notice(
            &StaleScheduledTask {
                task: task(Some(86_400)),
                outcome: StaleTaskOutcome::Rescheduled {
                    skipped: 1,
                    next_run_at: 1_700_000_000,
                },
            },
            chrono_tz::UTC,
        );

        assert!(notice.contains("1 occurrence was skipped"), "{notice}");
    }

    #[test]
    fn an_interval_is_described_in_its_largest_whole_unit() {
        let hourly = recurrence_note(&task(Some(3_600))).unwrap();
        let minutes = recurrence_note(&task(Some(300))).unwrap();
        let odd = recurrence_note(&task(Some(90))).unwrap();

        assert!(hourly.contains("1 hour(s)"), "{hourly}");
        assert!(minutes.contains("5 minute(s)"), "{minutes}");
        assert!(odd.contains("90 second(s)"), "{odd}");
    }
}
