//! Which services the list shows, in what order, and which rows stay as it changes.

use std::collections::HashMap;
use std::hash::Hash;

use burrow_core::monitor::Unit;

/// Services drawn before the rest are summed up.
pub(super) const MAX_ROWS: usize = 2000;

/// What a row of the services list stands for, so a new list finds the
/// row that showed it before.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum RowKey {
    /// A service, and which of the services with that name it is. Names
    /// come from the server, so they may repeat.
    Unit(String, usize),
    NoSystemd,
    NoneShown,
    More,
}

/// What a row of the services list shows.
#[derive(Debug, PartialEq)]
pub(super) enum Entry<'a> {
    Unit(&'a Unit),
    /// A note, and whether it is dimmed.
    Text(String, bool),
}

/// The rows of the services list: the services that pass the filter,
/// failed first, then running, then the rest, each by name, or a note
/// saying why there are none.
pub(super) fn entries<'a>(
    units: Option<&'a [Unit]>,
    systemd: bool,
    text: &str,
    failed_only: bool,
) -> Vec<(RowKey, Entry<'a>)> {
    let Some(units) = units else {
        if systemd {
            return Vec::new();
        }
        let note = Entry::Text("This server doesn't run systemd.".into(), false);
        return vec![(RowKey::NoSystemd, note)];
    };
    let mut shown: Vec<&Unit> = units
        .iter()
        .filter(|u| !failed_only || u.active == "failed")
        .filter(|u| {
            text.is_empty()
                || u.name.to_lowercase().contains(text)
                || u.description.to_lowercase().contains(text)
        })
        .collect();
    shown.sort_by_key(|u| (u.active != "failed", u.active != "active", &u.name));
    if shown.is_empty() {
        let note = if failed_only && text.is_empty() {
            "No service has failed."
        } else {
            "No service matches."
        };
        return vec![(RowKey::NoneShown, Entry::Text(note.into(), true))];
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut rows: Vec<(RowKey, Entry)> = shown
        .iter()
        .take(MAX_ROWS)
        .map(|unit| {
            let nth = seen.entry(&unit.name).or_default();
            *nth += 1;
            (RowKey::Unit(unit.name.clone(), *nth - 1), Entry::Unit(unit))
        })
        .collect();
    if shown.len() > MAX_ROWS {
        let more = format!("{} more. Filter to see them.", shown.len() - MAX_ROWS);
        rows.push((RowKey::More, Entry::Text(more, true)));
    }
    rows
}

/// Which rows of `old` can stay where they are when the list becomes
/// `new`: as many as possible whose order doesn't change. The others are
/// taken out and put back in their new place. Keys must not repeat.
pub(super) fn keep<K: Eq + Hash>(old: &[K], new: &[K]) -> Vec<bool> {
    let at: HashMap<&K, usize> = old.iter().enumerate().map(|(i, k)| (k, i)).collect();
    // The old places of the rows that are still there, in the new order.
    // The longest increasing run through them is what can stay.
    let places: Vec<usize> = new.iter().filter_map(|k| at.get(k).copied()).collect();
    // `tails[n]` is the index into `places` that ends the best run of
    // length n + 1 found so far, and `before` links each to the one
    // ahead of it.
    let mut tails: Vec<usize> = Vec::new();
    let mut before: Vec<Option<usize>> = vec![None; places.len()];
    for (i, &place) in places.iter().enumerate() {
        let n = tails.partition_point(|&t| places[t] < place);
        before[i] = n.checked_sub(1).map(|n| tails[n]);
        if n == tails.len() {
            tails.push(i);
        } else {
            tails[n] = i;
        }
    }
    let mut keep = vec![false; old.len()];
    let mut next = tails.last().copied();
    while let Some(i) = next {
        keep[places[i]] = true;
        next = before[i];
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, active: &str) -> Unit {
        Unit {
            name: name.into(),
            active: active.into(),
            ..Default::default()
        }
    }

    /// Checks `keep` against what the list ends up as: the rows kept are
    /// in the new order, and `kept` of them stay.
    fn check_keep(old: &[&str], new: &[&str], kept: usize) {
        let keep = keep(old, new);
        assert_eq!(keep.len(), old.len());
        let stay: Vec<&str> = old
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(o, _)| *o)
            .collect();
        assert_eq!(stay.len(), kept, "{old:?} to {new:?} keeps {stay:?}");
        let order: Vec<&str> = new.iter().copied().filter(|n| stay.contains(n)).collect();
        assert_eq!(stay, order);
    }

    #[test]
    fn rows_that_keep_their_order_stay() {
        check_keep(&["a", "b", "c"], &["a", "b", "c"], 3);
        check_keep(&["a", "b", "c"], &["a", "c"], 2);
        check_keep(&["a", "c"], &["a", "b", "c"], 2);
        check_keep(&[], &["a", "b"], 0);
        check_keep(&["a", "b"], &[], 0);
        // A service that fails moves to the top, and one that recovers
        // moves down. Only that row moves, not those it passes.
        check_keep(&["a", "b", "c", "d"], &["d", "a", "b", "c"], 3);
        check_keep(&["d", "a", "b", "c"], &["a", "b", "c", "d"], 3);
        check_keep(&["a", "b", "c", "d"], &["d", "c", "b", "a"], 1);
        check_keep(&["a", "b", "c", "d", "e"], &["b", "x", "a", "d", "e"], 3);
    }

    #[test]
    fn services_list_failed_then_running_then_the_rest() {
        let units = [
            unit("b.service", "inactive"),
            unit("c.service", "active"),
            unit("a.service", "active"),
            unit("d.service", "failed"),
        ];
        let keys: Vec<RowKey> = entries(Some(&units), true, "", false)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let key = |name: &str| RowKey::Unit(name.into(), 0);
        assert_eq!(
            keys,
            [
                key("d.service"),
                key("a.service"),
                key("c.service"),
                key("b.service")
            ]
        );
    }

    #[test]
    fn services_with_the_same_name_get_their_own_rows() {
        let units = [unit("a.service", "active"), unit("a.service", "active")];
        let keys: Vec<RowKey> = entries(Some(&units), true, "", false)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(
            keys,
            [
                RowKey::Unit("a.service".into(), 0),
                RowKey::Unit("a.service".into(), 1)
            ]
        );
    }

    #[test]
    fn services_list_says_why_it_is_empty() {
        assert_eq!(
            entries(None, false, "", false),
            [(
                RowKey::NoSystemd,
                Entry::Text("This server doesn't run systemd.".into(), false)
            )]
        );
        assert!(entries(None, true, "", false).is_empty());
        let units = [unit("a.service", "active")];
        assert_eq!(
            entries(Some(&units), true, "", true),
            [(
                RowKey::NoneShown,
                Entry::Text("No service has failed.".into(), true)
            )]
        );
        assert_eq!(
            entries(Some(&units), true, "nginx", false),
            [(
                RowKey::NoneShown,
                Entry::Text("No service matches.".into(), true)
            )]
        );
    }

    #[test]
    fn services_past_the_limit_are_counted() {
        let units: Vec<Unit> = (0..MAX_ROWS + 3)
            .map(|i| unit(&format!("u{i:05}.service"), "active"))
            .collect();
        let rows = entries(Some(&units), true, "", false);
        assert_eq!(rows.len(), MAX_ROWS + 1);
        assert_eq!(
            rows.last(),
            Some(&(
                RowKey::More,
                Entry::Text("3 more. Filter to see them.".into(), true)
            ))
        );
    }
}
