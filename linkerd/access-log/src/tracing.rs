use std::io::{self, Write};
use std::marker::PhantomData;
use tracing::subscriber::Interest;
use tracing::{Id, Metadata, Subscriber};
use tracing_subscriber::{
    fmt::{
        format::{DefaultFields, JsonFields},
        FormatFields, FormattedFields, MakeWriter,
    },
    layer::Context,
    registry::LookupSpan,
    Layer,
};

pub struct AccessLogWriter<W: MakeWriter, F = DefaultFields> {
    make_writer: W,
    _f: PhantomData<fn(F)>,
}

impl AccessLogWriter<fn() -> io::Stdout, DefaultFields> {
    pub fn new() -> Self {
        Self {
            make_writer: io::stdout,
            _f: PhantomData::default(),
        }
    }
}

impl Default for AccessLogWriter<fn() -> io::Stdout, DefaultFields> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W: MakeWriter, F> AccessLogWriter<W, F> {
    #[inline(always)]
    fn cares_about(&self, meta: &Metadata<'_>) -> bool {
        meta.target() == "access_log"
    }

    pub fn json(self) -> AccessLogWriter<W, JsonFields> {
        AccessLogWriter {
            make_writer: self.make_writer,
            _f: PhantomData::default(),
        }
    }

    pub fn with_writer<W2>(self, make_writer: W2) -> AccessLogWriter<W2, F>
    where
        W2: MakeWriter + 'static,
    {
        AccessLogWriter {
            make_writer,
            _f: Default::default(),
        }
    }
}

impl<S, W, F> Layer<S> for AccessLogWriter<W, F>
where
    W: MakeWriter + 'static,
    S: Subscriber + for<'span> LookupSpan<'span>,
    F: for<'writer> FormatFields<'writer> + 'static,
{
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if self.cares_about(metadata) {
            return Interest::always();
        }
        Interest::never()
    }

    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        self.cares_about(metadata)
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            if self.cares_about(span.metadata()) {
                if let Some(fields) = span.extensions().get::<FormattedFields<F>>() {
                    let mut writer = self.make_writer.make_writer();
                    let _ = writeln!(&mut writer, "{}", fields.fields);
                    println!("access log: {}", fields.fields);
                }
            }
        }
    }
}
