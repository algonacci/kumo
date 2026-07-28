//! Polls for due scheduled tasks and runs each one through the same agent loop as an ordinary
//! Telegram message, delivering the result to the chat that scheduled it.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use teloxide::prelude::*;
use tokio::sync::{Mutex, RwLock};

use crate::{
    AppState, PendingApprovals, PendingQuestions, logging, prepare_history,
    provider::Message as ProviderMessage, run_agent, send_formatted, send_mcp_images,
    storage::Database,
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

    // Tasks that missed their window by too long (e.g. Kumo was offline) are skipped rather than
    // run late and silently; tell the owning chat why nothing happened at the scheduled time.
    let stale = database
        .lock()
        .await
        .expire_stale_scheduled_tasks(now, STALE_AFTER.as_secs() as i64)?;
    for task in stale {
        let chat_id = ChatId(task.telegram_chat_id);
        let notice = format!(
            "\u{26a0}\u{fe0f} A scheduled task missed its time by more than an hour and was skipped: \"{}\"",
            task.prompt
        );
        if let Err(error) = bot.send_message(chat_id, notice).await {
            logging::warn(
                "scheduler",
                format!(
                    "task={} chat_id={} notification=failed reason=expired error={error:#}",
                    &task.id[..8],
                    chat_id
                ),
            );
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

/// The line appended to a recurring reminder telling the reader how to stop it. A reminder that
/// turns out to be unwanted is discovered *by being delivered*, so the way out belongs in the
/// message doing the interrupting — not in a command the reader has to already know about. One-shot
/// tasks get nothing: there is nothing to stop.
fn recurrence_note(task: &crate::storage::ScheduledTask) -> Option<String> {
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
    use crate::storage::ScheduledTask;

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
    fn an_interval_is_described_in_its_largest_whole_unit() {
        let hourly = recurrence_note(&task(Some(3_600))).unwrap();
        let minutes = recurrence_note(&task(Some(300))).unwrap();
        let odd = recurrence_note(&task(Some(90))).unwrap();

        assert!(hourly.contains("1 hour(s)"), "{hourly}");
        assert!(minutes.contains("5 minute(s)"), "{minutes}");
        assert!(odd.contains("90 second(s)"), "{odd}");
    }
}
