use crate::core::prompt::{build_history, system};
use crate::core::skill::get_all_skills;
use crate::core::tools::{shell_tool, subagent_tool};
use crate::providers::Model;
use aisdk::core::utils::step_count_is;
use aisdk::core::LanguageModelStreamChunkType;
use aisdk::core::{LanguageModelRequest, Message, Messages};
use anyhow::{Context, Result};
use futures::StreamExt;
use std::io::{self, Write};
use tracing::warn;

pub fn handle_list_skills() {
    let skills = get_all_skills();
    if skills.is_empty() {
        warn!("No skills found.");
        return;
    }
    println!("Available skills:");
    for s in &skills {
        println!(" - {}: {}", s.name, s.description);
    }
}

pub async fn handle_query(
    model: &mut Model,
    query: &str,
    history: Option<&mut Messages>,
) -> Result<()> {
    let skills = get_all_skills();

    let mut scan_sources: Vec<&str> = vec![query];
    if let Some(h) = &history {
        for msg in h.iter() {
            if let Message::User(u) = msg {
                scan_sources.push(&u.content);
            }
        }
    }
    let history_entries = build_history(history.as_ref().map(|h| h.as_slice()).unwrap_or_default());
    let sys = system(query, &skills, &scan_sources, &history_entries);

    tracing::debug!(prompt = %sys, query, "agent:");
    let mut req = {
        LanguageModelRequest::builder()
            .model(model.clone())
            .system(&sys)
            .prompt(query)
            .with_tool(shell_tool())
            .with_tool(subagent_tool(model.clone(), skills, history_entries))
            .stop_when(step_count_is(10))
            .build()
    };

    let mut response = req.stream_text().await.context("stream_text failed")?;

    while let Some(chunk) = response.stream.next().await {
        match chunk {
            LanguageModelStreamChunkType::TextDelta(text) => {
                print!("{}", text);
                io::stdout().flush()?;
            }
            LanguageModelStreamChunkType::Failed(e) => {
                tracing::error!("Stream error: {e}");
                break;
            }
            _ => {}
        }
    }

    println!();

    if let Some(h) = history {
        h.push(Message::User(query.into()));
        if let Some(text) = response.text().await {
            h.push(Message::Assistant(text.into()));
        }
    }

    Ok(())
}
