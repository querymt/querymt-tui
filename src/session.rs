use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::app::*;
use crate::protocol::*;

/// Returns indices of sessions in `group` whose title or ID contains `q`.
/// When `q` is empty every session matches.
fn matching_session_indices(group: &SessionGroup, q: &str) -> Vec<usize> {
    group
        .sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            q.is_empty()
                || s.title.as_deref().unwrap_or("").to_lowercase().contains(q)
                || s.session_id.to_lowercase().contains(q)
        })
        .map(|(i, _)| i)
        .collect()
}

impl App {
    /// Flat list of sessions that match the current filter, across all groups.
    ///
    /// Used by the session popup (which shows a flat list) for backward compatibility.
    pub fn filtered_sessions(&self) -> Vec<&SessionSummary> {
        let q = self.session_filter.to_lowercase();
        self.session_groups
            .iter()
            .flat_map(|g| {
                matching_session_indices(g, &q)
                    .into_iter()
                    .map(move |i| &g.sessions[i])
            })
            .collect()
    }

    /// Build the flat list of visible rows for the start-page session list.
    ///
    /// Each call re-evaluates the current `session_filter` and `collapsed_groups`.
    /// Group headers are always included; session rows are included only when
    /// the group is expanded *and* the session matches the filter.
    /// Groups with zero matching sessions are omitted entirely when a filter is
    /// active.
    pub fn visible_start_items(&self) -> Vec<StartPageItem> {
        let q = self.session_filter.to_lowercase();
        let mut items = Vec::new();

        // Cap the number of visible groups.
        let hidden_groups = self.session_groups.len().saturating_sub(MAX_VISIBLE_GROUPS);

        let groups_iter = self
            .session_groups
            .iter()
            .enumerate()
            .take(MAX_VISIBLE_GROUPS);

        for (group_idx, group) in groups_iter {
            let collapse_key = group.cwd.clone().unwrap_or_default();
            let collapsed = self.collapsed_groups.contains(&collapse_key);

            let matching = matching_session_indices(group, &q);

            // When a filter is active, skip groups with no matches entirely.
            if !q.is_empty() && matching.is_empty() {
                continue;
            }

            items.push(StartPageItem::GroupHeader {
                cwd: group.cwd.clone(),
                session_count: group.sessions.len(),
                collapsed,
            });

            if !collapsed {
                // Cap at MAX_RECENT_SESSIONS and append a ShowMore row if needed.
                let visible: Vec<usize> =
                    matching.iter().copied().take(MAX_RECENT_SESSIONS).collect();
                let hidden = matching.len().saturating_sub(MAX_RECENT_SESSIONS);

                for session_idx in visible {
                    items.push(StartPageItem::Session {
                        group_idx,
                        session_idx,
                    });
                }

                if hidden > 0 {
                    items.push(StartPageItem::ShowMore { remaining: hidden });
                }
            }
        }

        // Trailing ShowMore for hidden groups.
        if hidden_groups > 0 {
            items.push(StartPageItem::ShowMore {
                remaining: hidden_groups,
            });
        }

        items
    }

    /// Toggle the collapsed state of the group identified by `cwd`.
    ///
    /// `None` cwd is stored under the empty-string key so it can still be
    /// toggled independently.
    pub fn toggle_group_collapse(&mut self, cwd: Option<&str>) {
        let key = cwd.unwrap_or("").to_string();
        if !self.collapsed_groups.remove(&key) {
            self.collapsed_groups.insert(key);
        }
    }

    /// Toggle the collapsed state of a group *in the session popup*.
    ///
    /// Uses `popup_collapsed_groups` — fully independent of the start-page
    /// `collapsed_groups` so the two views never interfere.
    pub fn toggle_popup_group_collapse(&mut self, cwd: Option<&str>) {
        let key = cwd.unwrap_or("").to_string();
        if !self.popup_collapsed_groups.remove(&key) {
            self.popup_collapsed_groups.insert(key);
        }
    }

    /// Build the flat list of visible rows for the session popup.
    ///
    /// Mirrors [`visible_start_items`] but with two key differences:
    /// - Uses `popup_collapsed_groups` instead of `collapsed_groups`.
    /// - No `MAX_RECENT_SESSIONS` or `MAX_VISIBLE_GROUPS` caps — the popup
    ///   always shows every group and every session (its purpose is to browse
    ///   the full list).
    pub fn visible_popup_items(&self) -> Vec<PopupItem> {
        let q = self.session_filter.to_lowercase();
        let mut items = Vec::new();

        for (group_idx, group) in self.session_groups.iter().enumerate() {
            let collapse_key = group.cwd.clone().unwrap_or_default();
            let collapsed = self.popup_collapsed_groups.contains(&collapse_key);

            let matching = matching_session_indices(group, &q);

            // When a filter is active, skip groups with no matches entirely.
            if !q.is_empty() && matching.is_empty() {
                continue;
            }

            items.push(PopupItem::GroupHeader {
                cwd: group.cwd.clone(),
                session_count: group.sessions.len(),
                collapsed,
            });

            if !collapsed {
                for session_idx in matching {
                    items.push(PopupItem::Session {
                        group_idx,
                        session_idx,
                    });
                }
            }
        }

        items
    }

    pub fn resolve_new_session_default_cwd(&self) -> Option<String> {
        if let Some(active_session_id) = self.session_id.as_deref() {
            for group in &self.session_groups {
                for session in &group.sessions {
                    if session.session_id == active_session_id {
                        if let Some(cwd) = session.cwd.as_ref().filter(|cwd| !cwd.trim().is_empty())
                        {
                            return Some(cwd.clone());
                        }
                        if let Some(cwd) = group.cwd.as_ref().filter(|cwd| !cwd.trim().is_empty()) {
                            return Some(cwd.clone());
                        }
                    }
                }
            }
        }

        self.launch_cwd
            .as_ref()
            .filter(|cwd| !cwd.trim().is_empty())
            .cloned()
    }

    pub fn open_new_session_popup(&mut self) {
        self.popup = Popup::NewSession;
        self.new_session_path = self.resolve_new_session_default_cwd().unwrap_or_default();
        self.new_session_cursor = self.new_session_path.chars().count();
        self.refresh_new_session_completion();
    }

    pub fn new_session_base_dir(&self) -> PathBuf {
        self.launch_cwd
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn expand_user_path(&self, input: &str) -> PathBuf {
        if input == "~" {
            return dirs::home_dir().unwrap_or_else(|| PathBuf::from(input));
        }
        if let Some(rest) = input.strip_prefix("~/")
            && let Some(home) = dirs::home_dir()
        {
            return home.join(rest);
        }
        PathBuf::from(input)
    }

    fn normalize_lexical_path(&self, path: &Path) -> PathBuf {
        use std::path::Component;

        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
                Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !normalized.pop() {
                        normalized.push(Component::RootDir.as_os_str());
                    }
                }
                Component::Normal(part) => normalized.push(part),
            }
        }
        normalized
    }

    pub fn normalize_new_session_path(&self, input: &str) -> Option<String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return self.resolve_new_session_default_cwd().map(|cwd| {
                self.normalize_lexical_path(&PathBuf::from(cwd))
                    .to_string_lossy()
                    .into_owned()
            });
        }

        let path = self.expand_user_path(trimmed);
        let absolute = if path.is_absolute() {
            path
        } else {
            self.new_session_base_dir().join(path)
        };
        Some(
            self.normalize_lexical_path(&absolute)
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub fn collect_path_completion_candidates(&self, query: &str) -> Vec<FileIndexEntryLite> {
        let base_dir = self.new_session_base_dir();
        let typed = query.trim();
        let candidate_root = if typed.is_empty() {
            base_dir.clone()
        } else {
            let raw = PathBuf::from(typed);
            if raw.is_absolute() {
                raw.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("/"))
            } else {
                let joined = base_dir.join(raw);
                joined
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(base_dir.clone())
            }
        };

        let Ok(entries) = std::fs::read_dir(&candidate_root) else {
            return Vec::new();
        };

        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            candidates.push(FileIndexEntryLite {
                path: path.to_string_lossy().into_owned(),
                is_dir,
            });
        }
        candidates
    }

    pub fn rank_path_completion_matches(&self, query: &str) -> Vec<FileIndexEntryLite> {
        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(i64, bool, usize, FileIndexEntryLite)> = self
            .collect_path_completion_candidates(query)
            .into_iter()
            .filter_map(|entry| {
                let path = entry.path.as_str();
                let filename = path.rsplit('/').next().unwrap_or(path);
                let lower_path = path.to_lowercase();
                let lower_filename = filename.to_lowercase();
                let lower_query = query.trim().to_lowercase();

                let mut score = if lower_query.is_empty() {
                    0
                } else {
                    matcher
                        .fuzzy_match(path, query.trim())
                        .or_else(|| matcher.fuzzy_match(filename, query.trim()))?
                };
                if !lower_query.is_empty() && lower_path.starts_with(&lower_query) {
                    score += 10_000;
                }
                if !lower_query.is_empty() && lower_filename.starts_with(&lower_query) {
                    score += 7_500;
                }
                if !lower_query.is_empty() && lower_path.contains(&lower_query) {
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
            .take(6)
            .map(|(_, _, _, entry)| entry)
            .collect()
    }

    pub fn refresh_new_session_completion(&mut self) {
        let query = self.new_session_path.clone();
        let results = self.rank_path_completion_matches(&query);
        self.new_session_completion = Some(PathCompletionState {
            query,
            selected_index: 0,
            results,
        });
    }

    pub fn move_new_session_completion_selection(&mut self, delta: isize) {
        if let Some(completion) = self.new_session_completion.as_mut() {
            let len = completion.results.len();
            if len == 0 {
                completion.selected_index = 0;
                return;
            }
            let next =
                (completion.selected_index as isize + delta).rem_euclid(len as isize) as usize;
            completion.selected_index = next;
        }
    }

    pub fn accept_selected_new_session_completion(&mut self) -> bool {
        let Some(completion) = self.new_session_completion.clone() else {
            return false;
        };
        let Some(selected) = completion.results.get(completion.selected_index) else {
            return false;
        };

        let mut normalized = self
            .normalize_new_session_path(&selected.path)
            .unwrap_or_else(|| selected.path.clone());
        if selected.is_dir && !normalized.ends_with('/') {
            normalized.push('/');
        }
        self.new_session_path = normalized;
        self.new_session_cursor = self.new_session_path.len();
        self.new_session_completion = None;
        true
    }

    pub fn note_session_activity(&mut self, session_id: &str) {
        self.session_activity.insert(
            session_id.to_string(),
            SessionActivity {
                last_event_at: Instant::now(),
            },
        );
    }

    pub fn active_session_count(&self) -> usize {
        const ACTIVE_SESSION_WINDOW: Duration = Duration::from_secs(5);
        let now = Instant::now();
        self.session_activity
            .values()
            .filter(|activity| now.duration_since(activity.last_event_at) <= ACTIVE_SESSION_WINDOW)
            .count()
    }

    pub fn other_active_session_count(&self) -> usize {
        const ACTIVE_SESSION_WINDOW: Duration = Duration::from_secs(5);
        let now = Instant::now();
        self.session_activity
            .iter()
            .filter(|(session_id, activity)| {
                now.duration_since(activity.last_event_at) <= ACTIVE_SESSION_WINDOW
                    && self.session_id.as_deref() != Some(session_id.as_str())
            })
            .count()
    }
}
