use std::{fmt, sync::Arc};
use tracing::{field, span, Id, Level, Metadata, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    field::RecordFields,
    filter::{Directive, FilterFn, Filtered},
    fmt::{format, FormatFields, FormattedFields},
    layer::{Context, Layer},
    registry::LookupSpan,
};

const ENV_ACCESS_LOG: &str = "LINKERD2_PROXY_ACCESS_LOG";

pub const TRACE_TARGET: &str = "_access_log";

#[derive(Clone, Debug)]
pub struct Guard(Arc<WorkerGuard>);

pub(super) type AccessLogLayer<S> = Filtered<Writer, FilterFn, S>;

pub(super) struct Writer<F = ApacheCommon> {
    formatter: F,
}

#[derive(Default)]
pub(super) struct ApacheCommon {
    _p: (),
}

struct ApacheCommonVisitor<'writer> {
    res: fmt::Result,
    writer: format::Writer<'writer>,
}

pub(super) fn build<S>() -> Option<(AccessLogLayer<S>, Directive)>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    // Is access logging enabled?
    let _ = std::env::var(ENV_ACCESS_LOG).ok()?;

    // If access logging is enabled, build the access log layer.
    let writer = Writer::new().with_filter(
        FilterFn::new(
            (|meta| meta.level() == &Level::INFO && meta.target().starts_with(TRACE_TARGET))
                as fn(&Metadata<'_>) -> bool,
        )
        .with_max_level_hint(Level::INFO),
    );

    // Also, ensure that the `tracing` filter configuration will
    // always enable the access log spans.
    let directive = format!("{}=info", TRACE_TARGET)
        .parse()
        .expect("access logging filter directive must parse");

    Some((writer, directive))
}

// === impl Writer ===

impl Writer {
    pub fn new() -> Self {
        Self {
            formatter: Default::default(),
        }
    }
}

impl<S, F> Layer<S> for Writer<F>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    F: for<'writer> FormatFields<'writer> + 'static,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        let mut extensions = span.extensions_mut();

        if extensions.get_mut::<FormattedFields<F>>().is_none() {
            let mut fields = FormattedFields::<F>::new(String::new());
            if self
                .formatter
                .format_fields(fields.as_writer(), attrs)
                .is_ok()
            {
                extensions.insert(fields);
            }
        }
    }

    fn on_record(&self, id: &Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        let mut extensions = span.extensions_mut();
        if let Some(fields) = extensions.get_mut::<FormattedFields<F>>() {
            let _ = self.formatter.add_fields(fields, values);
            return;
        }

        let mut fields = FormattedFields::<F>::new(String::new());
        if self
            .formatter
            .format_fields(fields.as_writer(), values)
            .is_ok()
        {
            extensions.insert(fields);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            if let Some(fields) = span.extensions().get::<FormattedFields<F>>() {
                eprintln!("{}", fields.fields);
            }
        }
    }
}

impl ApacheCommon {
    const SKIPPED_FIELDS: &'static [&'static str] = &[
        "trace_id",
        "request_bytes",
        "total_ns",
        "processing_ns",
        "response_bytes",
        "user_agent",
        "host",
    ];
}

impl<'writer> FormatFields<'writer> for ApacheCommon {
    fn format_fields<R: RecordFields>(&self, writer: format::Writer<'_>, fields: R) -> fmt::Result {
        let mut visitor = ApacheCommonVisitor {
            writer,
            res: Ok(()),
        };
        fields.record(&mut visitor);
        visitor.res
    }

    #[inline]
    fn add_fields(
        &self,
        current: &mut FormattedFields<Self>,
        fields: &span::Record<'_>,
    ) -> fmt::Result {
        self.format_fields(current.as_writer(), fields)
    }
}

impl field::Visit for ApacheCommonVisitor<'_> {
    fn record_str(&mut self, field: &field::Field, val: &str) {
        self.record_debug(field, &format_args!("{}", val))
    }

    fn record_debug(&mut self, field: &field::Field, val: &dyn fmt::Debug) {
        self.res = match field.name() {
            n if ApacheCommon::SKIPPED_FIELDS.contains(&n) => return,
            "timestamp" => write!(&mut self.writer, " [{:?}]", val),
            "client.addr" => write!(&mut self.writer, "{:?}", val),
            "client.id" => write!(&mut self.writer, " {:?} -", val),
            "method" => write!(&mut self.writer, " \"{:?}", val),
            "version" => write!(&mut self.writer, " {:?}\"", val),
            _ => write!(&mut self.writer, " {:?}", val),
        }
    }
}
