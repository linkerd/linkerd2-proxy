use std::fmt;
use tracing::{field, span, Id, Level, Metadata, Subscriber};
use tracing_subscriber::{
    field::RecordFields,
    filter::{FilterFn, Filtered},
    fmt::{format, FormatFields, FormattedFields},
    layer::{Context, Layer},
    registry::LookupSpan,
};

pub const TRACE_TARGET: &str = "_access_log";

pub(super) type AccessLogLayer<S> =
    Filtered<Box<dyn Layer<S> + Send + Sync + 'static>, FilterFn, S>;

#[derive(Default)]
pub(super) struct Writer<F = ApacheCommon> {
    formatter: F,
}

#[derive(Default)]
pub(super) struct ApacheCommon {
    _p: (),
}

#[derive(Copy, Clone, Debug)]
pub(super) enum Format {
    Apache,
    Json,
}

struct ApacheCommonVisitor<'writer> {
    res: fmt::Result,
    writer: format::Writer<'writer>,
}

pub(super) fn build<S>(format: Format) -> AccessLogLayer<S>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    let writer: Box<dyn Layer<S> + Send + Sync + 'static> = match format {
        Format::Apache => Box::<Writer<ApacheCommon>>::default(),
        Format::Json => Box::<Writer<format::JsonFields>>::default(),
    };

    writer.with_filter(
        FilterFn::new(
            (|meta| meta.level() == &Level::INFO && meta.target().starts_with(TRACE_TARGET))
                as fn(&Metadata<'_>) -> bool,
        )
        .with_max_level_hint(Level::INFO),
    )
}

// === impl Writer ===

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

// === impl Format ===

impl std::str::FromStr for Format {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            s if s.eq_ignore_ascii_case("json") => Ok(Self::Json),
            s if s.eq_ignore_ascii_case("apache") => Ok(Self::Apache),
            _ => Err("expected either 'apache' or 'json'"),
        }
    }
}
