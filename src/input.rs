use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::app::{App, FileIndexEntryLite, MentionState};
use crate::protocol::ClientMsg;
use crate::ui::build_input_visual_layout;

impl App {
    // -- input helpers --

    fn reset_input_preferred_col(&mut self) {
        self.input_preferred_col = None;
    }

    pub fn input_insert(&mut self, c: char) {
        self.reset_input_preferred_col();
        self.input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
        self.refresh_mention_state();
    }

    pub fn input_backspace(&mut self) {
        if self.input_cursor > 0 {
            self.reset_input_preferred_col();
            let prev = self.input[..self.input_cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.drain(prev..self.input_cursor);
            self.input_cursor = prev;
            self.refresh_mention_state();
        }
    }

    pub fn input_delete(&mut self) {
        if self.input_cursor < self.input.len() {
            self.reset_input_preferred_col();
            let next = self.input[self.input_cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.input_cursor + i)
                .unwrap_or(self.input.len());
            self.input.drain(self.input_cursor..next);
            self.refresh_mention_state();
        }
    }

    pub fn input_left(&mut self) {
        if self.input_cursor > 0 {
            self.reset_input_preferred_col();
            self.input_cursor = self.input[..self.input_cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.refresh_mention_state();
        }
    }

    pub fn input_right(&mut self) {
        if self.input_cursor < self.input.len() {
            self.reset_input_preferred_col();
            self.input_cursor = self.input[self.input_cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.input_cursor + i)
                .unwrap_or(self.input.len());
            self.refresh_mention_state();
        }
    }

    pub fn input_home(&mut self) {
        self.reset_input_preferred_col();
        self.input_cursor = 0;
        self.refresh_mention_state();
    }

    pub fn input_end(&mut self) {
        self.reset_input_preferred_col();
        self.input_cursor = self.input.len();
        self.refresh_mention_state();
    }

    pub fn input_up_visual(&mut self, prefix_width: usize) {
        let layout = build_input_visual_layout(
            &self.input,
            self.input_cursor,
            self.input_line_width,
            prefix_width,
        );
        if layout.cursor_row == 0 {
            self.input_preferred_col = Some(layout.cursor_text_col);
            return;
        }
        let preferred_col = self.input_preferred_col.unwrap_or(layout.cursor_text_col);
        self.input_cursor = layout.cursor_offset_for_row_col(layout.cursor_row - 1, preferred_col);
        self.input_preferred_col = Some(preferred_col);
        self.refresh_mention_state();
    }

    pub fn input_down_visual(&mut self, prefix_width: usize) {
        let layout = build_input_visual_layout(
            &self.input,
            self.input_cursor,
            self.input_line_width,
            prefix_width,
        );
        if layout.cursor_row + 1 >= layout.total_rows() {
            self.input_preferred_col = Some(layout.cursor_text_col);
            return;
        }
        let preferred_col = self.input_preferred_col.unwrap_or(layout.cursor_text_col);
        self.input_cursor = layout.cursor_offset_for_row_col(layout.cursor_row + 1, preferred_col);
        self.input_preferred_col = Some(preferred_col);
        self.refresh_mention_state();
    }

    pub fn active_mention_query_from(&self, input: &str, cursor: usize) -> Option<(usize, String)> {
        if cursor > input.len() || !input.is_char_boundary(cursor) {
            return None;
        }

        let before_cursor = &input[..cursor];
        let trigger_start = before_cursor.rfind('@')?;
        let prefix = &before_cursor[..trigger_start];
        if !prefix.is_empty() && !prefix.ends_with(char::is_whitespace) {
            return None;
        }

        let token = &before_cursor[trigger_start + 1..];
        if token.chars().any(char::is_whitespace) {
            return None;
        }

        Some((trigger_start, token.to_string()))
    }

    pub fn rank_file_matches(&self, query: &str) -> Vec<FileIndexEntryLite> {
        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(i64, bool, usize, &FileIndexEntryLite)> = self
            .file_index
            .iter()
            .filter_map(|entry| {
                let path = entry.path.as_str();
                let filename = path.rsplit('/').next().unwrap_or(path);
                let lower_path = path.to_lowercase();
                let lower_filename = filename.to_lowercase();
                let lower_query = query.to_lowercase();

                let mut score = matcher.fuzzy_match(path, query)?;
                if query.is_empty() {
                    score = 0;
                }
                if !query.is_empty() && lower_path.starts_with(&lower_query) {
                    score += 10_000;
                }
                if !query.is_empty() && lower_filename.starts_with(&lower_query) {
                    score += 7_500;
                }
                if !query.is_empty() && lower_path.contains(&lower_query) {
                    score += 3_000;
                }

                Some((score, entry.is_dir, path.len(), entry))
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.path.cmp(&b.3.path))
        });

        scored
            .into_iter()
            .take(8)
            .map(|(_, _, _, entry)| entry.clone())
            .collect()
    }

    pub fn refresh_mention_state(&mut self) {
        let Some((trigger_start, query)) =
            self.active_mention_query_from(&self.input, self.input_cursor)
        else {
            self.mention_state = None;
            return;
        };

        let results = self.rank_file_matches(&query);
        self.mention_state = Some(MentionState {
            trigger_start,
            query,
            selected_index: 0,
            results,
        });
    }

    pub fn request_file_index_if_needed(&mut self) -> Option<ClientMsg> {
        if self.mention_state.is_some() && self.file_index.is_empty() && !self.file_index_loading {
            self.file_index_loading = true;
            self.file_index_error = None;
            return Some(ClientMsg::GetFileIndex);
        }
        None
    }

    pub fn move_mention_selection(&mut self, delta: isize) {
        if let Some(mention) = self.mention_state.as_mut() {
            let len = mention.results.len();
            if len == 0 {
                mention.selected_index = 0;
                return;
            }
            let next = (mention.selected_index as isize + delta).rem_euclid(len as isize) as usize;
            mention.selected_index = next;
        }
    }

    pub fn accept_selected_mention(&mut self) -> bool {
        let Some(mention) = self.mention_state.clone() else {
            return false;
        };
        let Some(selected) = mention.results.get(mention.selected_index).cloned() else {
            return false;
        };

        let replacement = format!("@{} ", selected.path);
        let replace_end = mention.trigger_start + 1 + mention.query.len();
        self.input
            .replace_range(mention.trigger_start..replace_end, &replacement);
        self.input_cursor = mention.trigger_start + replacement.len();
        self.mention_state = None;
        true
    }

    pub fn build_prompt_text_and_links(&self, input: &str) -> (String, Vec<String>) {
        let mut links = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let bytes = input.as_bytes();
        let mut i = 0usize;

        while i < bytes.len() {
            if bytes[i] == b'@' {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() {
                    let ch = input[end..].chars().next().unwrap_or(' ');
                    if ch.is_whitespace() {
                        break;
                    }
                    end += ch.len_utf8();
                }
                if end > start {
                    let candidate = &input[start..end];
                    let looks_like_path = candidate.contains('/') || candidate.contains('.');
                    if looks_like_path && seen.insert(candidate.to_string()) {
                        links.push(candidate.to_string());
                    }
                }
                i = end.max(i + 1);
                continue;
            }
            i += 1;
        }

        (input.to_string(), links)
    }

    pub fn take_input(&mut self) -> String {
        self.input_cursor = 0;
        self.input_scroll = 0;
        self.input_preferred_col = None;
        self.scroll_offset = 0;
        self.mention_state = None;
        std::mem::take(&mut self.input)
    }
}
