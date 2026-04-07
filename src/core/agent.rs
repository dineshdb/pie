use crate::core::prompt::system;
use crate::core::session::Session;
use crate::core::skill::get_all_skills;
use crate::core::tools::{shell_tool, subagent_tool};
use crate::providers::Model;
use aisdk::core::utils::step_count_is;
use aisdk::core::LanguageModelRequest;
use aisdk::core::LanguageModelStreamChunkType;
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

pub async fn handle_query(model: &mut Model, query: &str, session: &mut Session) -> Result<()> {
    let skills = get_all_skills();

    let mut scan_sources: Vec<&str> = vec![query];
    for entry in session.history_entries() {
        if entry.role == "user" {
            scan_sources.push(&entry.content);
        }
    }

    let history_entries = session.history_entries().to_vec();
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

    let mut assistant_text = String::new();
    while let Some(chunk) = response.stream.next().await {
        match chunk {
            LanguageModelStreamChunkType::TextDelta(text) => {
                print!("{}", text);
                assistant_text.push_str(&text);
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

    session.add_user(query)?;
    if !assistant_text.is_empty() {
        session.add_assistant(&assistant_text)?;
    }

    Ok(())
}
