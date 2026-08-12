//! Dérivation automatique du titre de session.
pub const MAX_TITLE_CHARS: usize = 50;

pub fn derive_title(first_user_message: &str) -> String {
    let trimmed = first_user_message.trim();
    let single_line = trimmed.split('\n').next().unwrap_or("").trim();
    if single_line.is_empty() { return "Nouvelle session".to_string(); }
    let char_count = single_line.chars().count();
    if char_count <= MAX_TITLE_CHARS { return single_line.to_string(); }
    let cutoff = single_line.char_indices().nth(MAX_TITLE_CHARS - 1).map(|(i, _)| i).unwrap_or(single_line.len());
    format!("{}…", &single_line[..cutoff])
}

#[cfg(test)]
#[path = "../test/title.rs"]
mod tests;
