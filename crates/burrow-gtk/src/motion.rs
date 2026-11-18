//! What moves on its own follows the desktop's animations setting. GTK
//! 4.24's spinner turns whatever that setting says, so the app checks it
//! before starting one.

use relm4::gtk;
use relm4::gtk::prelude::*;

/// How a spinner looks while something is under way.
#[derive(Debug, PartialEq)]
struct Look {
    shown: bool,
    spinning: bool,
}

/// With animations off the spinner stays hidden, as it is when nothing is
/// under way, since one that isn't turning draws nothing but a gap. The
/// message beside it still says what is happening.
fn look(animations: bool) -> Look {
    Look {
        shown: animations,
        spinning: animations,
    }
}

/// Shows and starts `spinner`, unless the desktop has animations off.
pub fn spin(spinner: &gtk::Spinner) {
    let look = look(spinner.settings().is_gtk_enable_animations());
    spinner.set_visible(look.shown);
    spinner.set_spinning(look.spinning);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spinner_turns_only_with_animations_on() {
        let on = Look {
            shown: true,
            spinning: true,
        };
        let off = Look {
            shown: false,
            spinning: false,
        };
        assert_eq!(look(true), on);
        assert_eq!(look(false), off);
    }
}
