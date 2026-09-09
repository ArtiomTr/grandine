//! A `tracing` layer that sums how long each span was on the stack.
//!
//! `tracing-subscriber`'s own `FmtSpan::CLOSE` prints one line per span close, which buries a
//! breakdown under thousands of lines when a read walks a whole delta chain. This aggregates
//! instead, and reports self time - a span's own time with the time of its children taken out -
//! next to total time, so that a phase can be read off directly.

use core::time::Duration;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use tracing::{
    Id, Subscriber,
    field::{Field, Visit},
    span::Attributes,
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

#[derive(Default)]
struct SpanTiming {
    label: String,
    entered_at: Option<Instant>,
    busy: Duration,
    child_busy: Duration,
}

/// Picks up the `step` field that epoch processing tags its spans with, so that the steps show up
/// separately rather than collapsing into one `epoch_step` row.
#[derive(Default)]
struct StepVisitor(Option<String>);

impl Visit for StepVisitor {
    fn record_debug(&mut self, _field: &Field, _value: &dyn core::fmt::Debug) {}

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "step" {
            self.0 = Some(value.to_owned());
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct Totals {
    pub calls: u64,
    pub total: Duration,
    pub own: Duration,
}

#[derive(Default)]
struct State {
    spans: HashMap<u64, SpanTiming>,
    totals: HashMap<String, Totals>,
    order: Vec<String>,
}

/// Cheap to clone: every clone shares the same totals, so one can be handed to the subscriber and
/// another kept to read the results off.
#[derive(Clone, Default)]
pub struct TimingLayer {
    state: Arc<Mutex<State>>,
}

impl TimingLayer {
    /// Everything measured so far, in the order the span names were first seen.
    pub fn drain(&self) -> Vec<(String, Totals)> {
        let mut state = self.state.lock().expect("timing state is never poisoned");
        let order = core::mem::take(&mut state.order);
        let totals = core::mem::take(&mut state.totals);

        state.spans.clear();

        order
            .into_iter()
            .filter_map(|name| totals.get(&name).map(|totals| (name, *totals)))
            .collect()
    }
}

impl<S> Layer<S> for TimingLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, _context: Context<'_, S>) {
        let mut visitor = StepVisitor::default();

        attributes.record(&mut visitor);

        let name = attributes.metadata().name();
        let label = match visitor.0 {
            Some(step) => format!("{name}: {step}"),
            None => name.to_owned(),
        };

        self.state
            .lock()
            .expect("timing state is never poisoned")
            .spans
            .insert(
                id.into_u64(),
                SpanTiming {
                    label,
                    ..SpanTiming::default()
                },
            );
    }

    fn on_enter(&self, id: &Id, _context: Context<'_, S>) {
        if let Some(span) = self
            .state
            .lock()
            .expect("timing state is never poisoned")
            .spans
            .get_mut(&id.into_u64())
        {
            span.entered_at = Some(Instant::now());
        }
    }

    fn on_exit(&self, id: &Id, _context: Context<'_, S>) {
        let mut state = self.state.lock().expect("timing state is never poisoned");

        if let Some(span) = state.spans.get_mut(&id.into_u64())
            && let Some(entered_at) = span.entered_at.take()
        {
            span.busy = span.busy.saturating_add(entered_at.elapsed());
        }
    }

    fn on_close(&self, id: Id, context: Context<'_, S>) {
        let Some(reference) = context.span(&id) else {
            return;
        };

        let parent = reference.parent().map(|parent| parent.id().into_u64());

        let mut state = self.state.lock().expect("timing state is never poisoned");

        let Some(span) = state.spans.remove(&id.into_u64()) else {
            return;
        };

        let own = span.busy.saturating_sub(span.child_busy);

        if !state.totals.contains_key(&span.label) {
            state.order.push(span.label.clone());
        }

        let totals = state.totals.entry(span.label).or_default();

        totals.calls = totals.calls.saturating_add(1);
        totals.total = totals.total.saturating_add(span.busy);
        totals.own = totals.own.saturating_add(own);

        if let Some(parent) = parent.and_then(|parent| state.spans.get_mut(&parent)) {
            parent.child_busy = parent.child_busy.saturating_add(span.busy);
        }
    }
}
