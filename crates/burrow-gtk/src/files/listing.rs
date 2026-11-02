//! Filling the list from a folder's entries, touching only the rows that changed.

use std::cmp::Ordering;

use burrow_core::{FileEntry, FileKind};
use relm4::gtk;
use relm4::gtk::prelude::*;

use super::Files;
use super::page::row_content;

/// Rows drawn before the rest of a folder is summed up. A list of widgets
/// gets slow long before a folder gets this big.
pub(super) const MAX_ROWS: usize = 2000;

impl Files {
    /// Draws `entries`, selecting the one called `keep`. A new listing of
    /// the same folder only touches the rows that changed, so the scroll
    /// position, the selection and the focus stay where they were.
    pub(super) fn fill_list(
        &mut self,
        entries: Vec<FileEntry>,
        same_folder: bool,
        keep: Option<&str>,
    ) {
        let had_focus = self.list.focus_child().is_some();
        let old = if same_folder {
            shown(&self.entries)
        } else {
            self.list.remove_all();
            self.list.append(&self.more_row);
            self.scroller.vadjustment().set_value(0.0);
            &[]
        };
        for op in diff(old, shown(&entries)) {
            match op {
                Op::Remove(at) => {
                    if let Some(row) = self.list.row_at_index(at as i32) {
                        self.list.remove(&row);
                    }
                }
                Op::Insert(at) => {
                    let row = gtk::ListBoxRow::new();
                    row.set_child(Some(&row_content(&entries[at])));
                    self.list.insert(&row, at as i32);
                }
                // The row itself stays, and with it the selection and focus.
                Op::Replace(at) => {
                    if let Some(row) = self.list.row_at_index(at as i32) {
                        row.set_child(Some(&row_content(&entries[at])));
                    }
                }
            }
        }
        self.entries = entries;
        let hidden = self.entries.len().saturating_sub(MAX_ROWS);
        self.more
            .set_label(&format!("…and {hidden} more, not shown"));
        self.more_row.set_visible(hidden > 0);

        // Removing the focused row makes GTK select whichever row it
        // focuses next, so the selection is set again either way.
        let index = keep.and_then(|name| self.entries.iter().position(|e| e.name == name));
        match index
            .filter(|&i| i < MAX_ROWS)
            .and_then(|i| self.list.row_at_index(i as i32))
        {
            Some(row) => {
                if !row.is_selected() {
                    self.list.select_row(Some(&row));
                }
                if had_focus && self.list.focus_child().is_none() {
                    row.grab_focus();
                }
            }
            None => self.list.unselect_all(),
        }
        let selected = self.selected().is_some();
        for button in &self.on_selection {
            button.set_sensitive(selected);
        }
    }
}

/// The entries that get a row.
pub(super) fn shown(entries: &[FileEntry]) -> &[FileEntry] {
    &entries[..entries.len().min(MAX_ROWS)]
}

/// One step from the rows of one listing to the rows of the next. `at` is
/// both the row's position at that step and its index in the new entries.
#[derive(Debug, PartialEq)]
pub(super) enum Op {
    Remove(usize),
    Insert(usize),
    Replace(usize),
}

/// The steps from the rows for `old` to the rows for `new`, both sorted by
/// [`order`]. Rows for entries that didn't change are left alone.
pub(super) fn diff(old: &[FileEntry], new: &[FileEntry]) -> Vec<Op> {
    let (mut i, mut at, mut ops) = (0, 0, Vec::new());
    while i < old.len() || at < new.len() {
        let step = match (old.get(i), new.get(at)) {
            (Some(a), Some(b)) => order(a, b),
            (Some(_), None) => Ordering::Less,
            (None, _) => Ordering::Greater,
        };
        match step {
            Ordering::Less => {
                ops.push(Op::Remove(at));
                i += 1;
            }
            Ordering::Greater => {
                ops.push(Op::Insert(at));
                at += 1;
            }
            Ordering::Equal => {
                if old[i] != new[at] {
                    ops.push(Op::Replace(at));
                }
                i += 1;
                at += 1;
            }
        }
    }
    ops
}

/// Folders first, then by name.
pub(super) fn order(a: &FileEntry, b: &FileEntry) -> Ordering {
    (a.kind != FileKind::Dir)
        .cmp(&(b.kind != FileKind::Dir))
        .then_with(|| a.name.cmp(&b.name))
}

pub(super) fn sorted(mut entries: Vec<FileEntry>) -> Vec<FileEntry> {
    entries.sort_by(order);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: FileKind) -> FileEntry {
        FileEntry {
            name: name.into(),
            kind,
            size: 0,
            modified: None,
            permissions: None,
        }
    }

    #[test]
    fn folders_sort_first() {
        let names: Vec<String> = sorted(vec![
            entry("b.txt", FileKind::File),
            entry("z", FileKind::Dir),
            entry("a.txt", FileKind::File),
        ])
        .into_iter()
        .map(|e| e.name)
        .collect();
        assert_eq!(names, ["z", "a.txt", "b.txt"]);
    }

    /// Runs `diff`'s steps on a copy of `old`, as `fill_list` does on rows.
    fn apply(old: &[FileEntry], new: &[FileEntry]) -> (Vec<FileEntry>, Vec<Op>) {
        let ops = diff(old, new);
        let mut rows = old.to_vec();
        for op in &ops {
            match *op {
                Op::Remove(at) => {
                    rows.remove(at);
                }
                Op::Insert(at) => rows.insert(at, new[at].clone()),
                Op::Replace(at) => rows[at] = new[at].clone(),
            }
        }
        (rows, ops)
    }

    #[test]
    fn listing_again_only_touches_what_changed() {
        let old = sorted(vec![
            entry("docs", FileKind::Dir),
            entry("a.txt", FileKind::File),
            entry("b.txt", FileKind::File),
            entry("c.txt", FileKind::File),
        ]);
        assert_eq!(diff(&old, &old), []);

        let mut grown = entry("b.txt", FileKind::File);
        grown.size = 10;
        let new = sorted(vec![
            entry("docs", FileKind::Dir),
            entry("aa.txt", FileKind::File),
            grown,
            entry("c.txt", FileKind::File),
            entry("d.txt", FileKind::File),
        ]);
        let (rows, ops) = apply(&old, &new);
        assert_eq!(rows, new);
        assert_eq!(
            ops,
            [Op::Remove(1), Op::Insert(1), Op::Replace(2), Op::Insert(4)]
        );
    }

    #[test]
    fn a_name_that_changes_kind_moves() {
        let old = sorted(vec![entry("x", FileKind::File), entry("y", FileKind::File)]);
        let new = sorted(vec![entry("x", FileKind::Dir), entry("y", FileKind::File)]);
        let (rows, _) = apply(&old, &new);
        assert_eq!(rows, new);
    }

    #[test]
    fn diffs_reach_any_listing() {
        let all: Vec<FileEntry> = ["a", "b", "c", "d"]
            .iter()
            .flat_map(|n| [entry(n, FileKind::File), entry(n, FileKind::Dir)])
            .collect();
        // Every pair of subsets, as a stand-in for any two listings.
        let subset = |bits: u32| {
            sorted(
                (0..all.len())
                    .filter(|i| bits & (1 << i) != 0)
                    .map(|i| all[i].clone())
                    .collect(),
            )
        };
        for a in (0..256).step_by(7) {
            for b in (0..256).step_by(5) {
                let (old, new) = (subset(a), subset(b));
                assert_eq!(apply(&old, &new).0, new, "{a:#b} to {b:#b}");
            }
        }
    }

    #[test]
    fn only_drawn_rows_are_diffed() {
        let many: Vec<FileEntry> = (0..MAX_ROWS + 5)
            .map(|i| entry(&format!("{i:05}"), FileKind::File))
            .collect();
        assert_eq!(shown(&many).len(), MAX_ROWS);
        assert_eq!(diff(shown(&many), shown(&many[1..])).len(), 2);
    }
}
