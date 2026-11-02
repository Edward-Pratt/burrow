//! The meters and charts the samples are drawn on.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::files::size;

use super::page::label;

/// Samples the charts show: ten minutes, at one every 2 seconds.
pub(super) const CHART_POINTS: usize = 300;

/// A bar with "used of total" next to it.
pub(super) struct Meter {
    pub(super) bar: gtk::LevelBar,
    pub(super) text: gtk::Label,
    /// Both, for meters that stand alone.
    pub(super) row: gtk::Box,
}

impl Meter {
    pub(super) fn new() -> Meter {
        let bar = gtk::LevelBar::new();
        bar.set_hexpand(true);
        bar.set_valign(gtk::Align::Center);
        // Full is the warning, so the theme's "high" colour isn't used for
        // a disk that is merely busy.
        bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_HIGH));
        bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_LOW, 0.9);
        bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_HIGH, 0.9);
        bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_FULL, 1.0);
        let text = label("");
        text.set_width_chars(24);
        Meter {
            bar,
            text,
            row: gtk::Box::new(gtk::Orientation::Horizontal, 12),
        }
    }

    /// The bar and text with `name` before them, in [`Meter::row`].
    pub(super) fn row(&self, name: &str) -> gtk::Box {
        let name = label(name);
        name.set_width_chars(8);
        self.row.append(&name);
        self.row.append(&self.bar);
        self.row.append(&self.text);
        self.row.clone()
    }

    pub(super) fn set(&self, used: u64, total: u64) {
        let share = if total == 0 {
            0.0
        } else {
            used as f64 / total as f64
        };
        self.bar.set_value(share);
        self.text.set_label(&format!(
            "{} of {} ({:.0}%)",
            size(used),
            size(total),
            share * 100.0
        ));
    }
}

/// A line chart of the last [`CHART_POINTS`] values of one or more series.
pub(super) struct Chart {
    pub(super) area: gtk::DrawingArea,
    pub(super) data: Rc<RefCell<ChartData>>,
}

pub(super) struct ChartData {
    pub(super) series: Vec<VecDeque<f64>>,
    /// Drawn instead of `series` while set: the history on disk, and how
    /// many points the width stands for.
    pub(super) fixed: Option<(Vec<Vec<f64>>, usize)>,
    pub(super) colors: Vec<gtk::gdk::RGBA>,
    pub(super) max: f64,
}

impl ChartData {
    /// What is drawn, and how many points the width stands for.
    pub(super) fn shown(&self) -> (Vec<Vec<f64>>, usize) {
        match &self.fixed {
            Some((fixed, points)) => (fixed.clone(), (*points).max(2)),
            None => (
                self.series
                    .iter()
                    .map(|s| s.iter().copied().collect())
                    .collect(),
                CHART_POINTS,
            ),
        }
    }
}

impl Chart {
    pub(super) fn new(colors: &[&str]) -> Chart {
        let data = Rc::new(RefCell::new(ChartData {
            series: vec![VecDeque::new(); colors.len()],
            fixed: None,
            colors: colors
                .iter()
                .map(|c| gtk::gdk::RGBA::parse(*c).expect("a valid colour"))
                .collect(),
            max: 1.0,
        }));
        let area = gtk::DrawingArea::new();
        area.set_content_height(120);
        area.set_hexpand(true);
        area.set_draw_func({
            let data = data.clone();
            move |area, cr, width, height| draw(area, cr, width, height, &data.borrow())
        });
        Chart { area, data }
    }

    pub(super) fn push(&self, values: &[f64]) {
        let mut data = self.data.borrow_mut();
        for (series, value) in data.series.iter_mut().zip(values) {
            if series.len() == CHART_POINTS {
                series.pop_front();
            }
            series.push_back(*value);
        }
    }

    /// Draws `fixed` instead of the live values, as the newest of `points`
    /// across the width, or the live values again.
    pub(super) fn show(&self, fixed: Option<(Vec<Vec<f64>>, usize)>) {
        self.data.borrow_mut().fixed = fixed;
        self.area.queue_draw();
    }

    /// The highest value shown.
    pub(super) fn peak(&self) -> f64 {
        let (series, _) = self.data.borrow().shown();
        series.iter().flatten().copied().fold(0.0, f64::max)
    }

    pub(super) fn set_max(&self, max: f64) {
        self.data.borrow_mut().max = max;
        self.area.queue_draw();
    }
}

pub(super) fn draw(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    data: &ChartData,
) {
    let (width, height) = (f64::from(width), f64::from(height));
    let ink = area.color();
    // A faint frame and quarter lines, in the text colour so they suit
    // light and dark themes.
    cr.set_source_rgba(
        ink.red().into(),
        ink.green().into(),
        ink.blue().into(),
        0.12,
    );
    cr.set_line_width(1.0);
    for quarter in 0..=4 {
        let y = (height - 1.0) * f64::from(quarter) / 4.0 + 0.5;
        cr.move_to(0.0, y);
        cr.line_to(width, y);
    }
    let _ = cr.stroke();

    let (shown, points) = data.shown();
    let step = width / (points - 1) as f64;
    for (series, color) in shown.iter().zip(&data.colors) {
        // The newest point is at the right edge.
        let offset = points.saturating_sub(series.len());
        for (i, value) in series.iter().enumerate() {
            let x = (offset + i) as f64 * step;
            let y = height - (value / data.max).clamp(0.0, 1.0) * (height - 2.0) - 1.0;
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            1.0,
        );
        cr.set_line_width(1.5);
        let _ = cr.stroke();
    }
}
