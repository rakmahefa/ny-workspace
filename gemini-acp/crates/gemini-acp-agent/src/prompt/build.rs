//! Assemblage du prompt multi-tour (spec §3.4 + refactor M8 §4.2).

use gemini_acp_runtime::persona;
use gemini_acp_runtime::state::{Role, Session};
use gemini_acp_runtime::tools::ToolRegistry;

pub const MAX_MESSAGES: usize = 12;
pub const MAX_PROMPT_CHARS: usize = 32_000;

pub fn build_prompt(session: &Session, registry: Option<&ToolRegistry>) -> String {
    let system = persona::system_prompt(session, None);
    let tools_section = if session.tools_enabled {
        registry.and_then(gemini_acp_runtime::tools::prompt::tools_section)
    } else {
        None
    };
    let system = match tools_section {
        Some(ts) => format!("{system}{ts}\n\n"),
        None => system,
    };
    let n = session.messages.len();
    if n == 0 { return system; }
    let lens: Vec<usize> = session.messages.iter().map(|(role, text)| {
        let tag = match role { Role::User => "[User]", Role::Assistant => "[Assistant]", Role::Tool => "[Tool result]" };
        tag.len() + 2 + text.chars().count() + 2
    }).collect();
    let mut prefix = vec![0usize; n + 1];
    for i in 0..n { prefix[i + 1] = prefix[i] + lens[i]; }
    let min_start_msg = n.saturating_sub(MAX_MESSAGES);
    let budget_ok = |start: usize| prefix[n] - prefix[start] <= MAX_PROMPT_CHARS;
    let mut lo = min_start_msg;
    let mut hi = n - 1;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if budget_ok(mid) { hi = mid; } else { lo = mid + 1; }
    }
    format!("{system}{}", format_history(session, lo))
}

fn format_history(session: &Session, start: usize) -> String {
    let mut out = String::new();
    for (role, text) in session.messages.iter().skip(start) {
        let tag = match role { Role::User => "[User]", Role::Assistant => "[Assistant]", Role::Tool => "[Tool result]" };
        out.push_str(tag);
        out.push_str(": ");
        out.push_str(text);
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
#[path = "../test/build.rs"]
mod tests;
