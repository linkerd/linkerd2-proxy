use std::{fmt, io::Write, path::PathBuf, sync::Arc};
use tracing::{field, span, Id, Level, Metadata, Subscriber};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{
    field::RecordFields,
    filter::{Directive, FilterFn, Filtered},
    fmt::{FormatFields, FormattedFields, MakeWriter},
    layer::{Context, Layer},
    registry::LookupSpan,
};

const ENV_ACCESS_LOG: &str = "LINKERD2_PROXY_ACCESS_LOG";

pub const TRACE_TARGET: &str = "_access_log";

#[derive(Clone, Debug)]
pub struct Guard(Arc<WorkerGuard>);

pub(super) type AccessLogLayer<S> = Filtered<Writer, FilterFn, S>;

pub(super) struct Writer<F = ApacheCommon> {
    make_writer: NonBlocking,
    formatter: F,
}

#[derive(Default)]
pub(super) struct ApacheCommon {
    _p: (),
}

struct ApacheCommonVisitor<'writer> {
    res: fmt::Result,
    writer: &'writer mut dyn fmt::Write,
}

pub(super) fn build<S>() -> Option<(AccessLogLayer<S>, Guard, Directive)>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    // Create the access log file, or open it in append-only mode if
    // it already exists.
    let file = {
        let path = {
            let p = std::env::var(ENV_ACCESS_LOG).ok()?;
            p.parse::<PathBuf>()
                .map_err(|e| eprintln!("{} is not a valid path: {}", p, e))
                .ok()?
        };

        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| eprintln!("Failed to open file {}: {}", path.display(), e,))
            .ok()?
    };

    // If we successfully created or opened the access log file,
    // build the access log layer.
    eprintln!("Writing access log to {:?}", file);
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let writer = Writer::new(non_blocking).with_filter(
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

    Some((writer, Guard(guard.into()), directive))
}

// === impl Writer ===

impl Writer {
    pub fn new(make_writer: NonBlocking) -> Self {
        Self {
            make_writer,
            formatter: Default::default(),
        }
    }
}

impl<S, F> Layer<S> for Writer<F>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    F: for<'writer> FormatFields<'writer> + 'static,
{
    fn new_span(&self, attrs: &span::Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        let mut extensions = span.extensions_mut();

        if extensions.get_mut::<FormattedFields<F>>().is_none() {
            let mut buf = String::new();
            if self.formatter.format_fields(&mut buf, attrs).is_ok() {
                let fmt_fields = FormattedFields::<F>::new(buf);
                extensions.insert(fmt_fields);
            }
        }
    }

    fn on_record(&self, id: &Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        let mut extensions = span.extensions_mut();
        if let Some(FormattedFields { ref mut fields, .. }) =
            extensions.get_mut::<FormattedFields<F>>()
        {
            let _ = self.formatter.add_fields(fields, values);
            return;
        }

        let mut buf = String::new();
        if self.formatter.format_fields(&mut buf, values).is_ok() {
            let fmt_fields = FormattedFields::<F>::new(buf);
            extensions.insert(fmt_fields);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            if let Some(fields) = span.extensions().get::<FormattedFields<F>>() {
                let mut writer = self.make_writer.make_writer();
                let _ = writeln!(&mut writer, "{}", fields.fields);
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
    fn format_fields<R: RecordFields>(
        &self,
        writer: &'writer mut dyn fmt::Write,
        fields: R,
    ) -> fmt::Result {
        let mut visitor = ApacheCommonVisitor {
            writer,
            res: Ok(()),
        };
        fields.record(&mut visitor);
        visitor.res
    }

    fn add_fields(&self, current: &'writer mut String, fields: &span::Record<'_>) -> fmt::Result {
        self.format_fields(current, fields)
    }
}

impl field::Visit for ApacheCommonVisitor<'_> {
    fn record_str(&mut self, field: &field::Field, val: &str) {
        self.record_debug(field, &format_args!("{}", val))
    }

    fn record_debug(&mut self, field: &field::Field, val: &dyn fmt::Debug) {
        use fmt::Write;
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
