use env_logger;
use futures::future::{ExecuteError, Executor};
use futures::{Future, Poll};
use log::Level;
use std::cell::RefCell;
use std::env;
use std::fmt;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_timer::clock;

use task;

const ENV_LOG: &str = "LINKERD2_PROXY_LOG";

thread_local! {
    static CONTEXT: RefCell<Vec<*const fmt::Display>> = RefCell::new(Vec::new());
}

pub fn dispatch() -> trace::Dispatch {
    let s = trace_fmt::FmtSubscriber::builder()
        // .on_event(fmt_event)
        .full()
        .with_filter(trace_fmt::filter::EnvFilter::from_env(ENV_LOG))
        .finish();
    trace::Dispatch::new(s)
}

pub fn formatted_builder() -> env_logger::Builder {
    let start_time = clock::now();
    let mut builder = env_logger::Builder::new();
    builder.format(move |fmt, record| {
        CONTEXT.with(move |ctxt| {
            let level = match record.level() {
                Level::Trace => "TRCE",
                Level::Debug => "DBUG",
                Level::Info => "INFO",
                Level::Warn => "WARN",
                Level::Error => "ERR!",
            };
            let uptime = clock::now() - start_time;
            writeln!(
                fmt,
                "{} [{:>6}.{:06}s] {}{} {}",
                level,
                uptime.as_secs(),
                uptime.subsec_micros(),
                Context(&ctxt.borrow()),
                record.target(),
                record.args()
            )
        })
    });
    builder
}

pub fn init() {
    formatted_builder()
        .parse(&env::var(ENV_LOG).unwrap_or_default())
        .init();
}

/// Execute a closure with a `Display` item attached to allow log messages.
pub fn context<T, F, U>(context: &T, mut closure: F) -> U
where
    T: fmt::Display + 'static,
    F: FnMut() -> U,
{
    let _guard = ContextGuard::new(context);
    closure()
}

/// Wrap a `Future` with a `Display` value that will be inserted into all logs
/// created by this Future.
pub fn context_future<T: fmt::Display, F: Future>(context: T, future: F) -> ContextualFuture<T, F> {
    ContextualFuture {
        context,
        future: Some(future),
    }
}

/// Wrap `task::LazyExecutor` to spawn futures that have a reference to the `Display`
/// value, inserting it into all logs created by this future.
pub fn context_executor<T: fmt::Display>(context: T) -> ContextualExecutor<T> {
    ContextualExecutor {
        context: Arc::new(context),
    }
}

#[derive(Debug)]
pub struct ContextualFuture<T: fmt::Display + 'static, F: Future> {
    context: T,
    future: Option<F>,
}

impl<T, F> Future for ContextualFuture<T, F>
where
    T: fmt::Display + 'static,
    F: Future,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let ctxt = &self.context;
        let fut = self.future.as_mut().expect("poll after drop");
        context(ctxt, || fut.poll())
    }
}
impl<T, F> Drop for ContextualFuture<T, F>
where
    T: fmt::Display + 'static,
    F: Future,
{
    fn drop(&mut self) {
        if self.future.is_some() {
            let ctxt = &self.context;
            let fut = &mut self.future;
            context(ctxt, || drop(fut.take()))
        }
    }
}

#[derive(Debug)]
pub struct ContextualExecutor<T> {
    context: Arc<T>,
}

impl<C, T> task::TypedExecutor<T> for ContextualExecutor<C>
where
    T: Future<Item = (), Error = ()> + Send + 'static,
    C: fmt::Display + 'static + Send + Sync,
{
    fn spawn(&mut self, future: T) -> Result<(), tokio::executor::SpawnError> {
        let fut = context_future(self.context.clone(), future);
        ::task::LazyExecutor.spawn(fut)
    }
}

impl<T> task::TokioExecutor for ContextualExecutor<T>
where
    T: fmt::Display + 'static + Send + Sync,
{
    fn spawn(
        &mut self,
        future: Box<Future<Item = (), Error = ()> + 'static + Send>,
    ) -> Result<(), ::tokio::executor::SpawnError> {
        let fut = context_future(self.context.clone(), future);
        task::LazyExecutor.spawn(Box::new(fut))
    }
}

impl<T, F> Executor<F> for ContextualExecutor<T>
where
    T: fmt::Display + 'static + Send + Sync,
    F: Future<Item = (), Error = ()> + 'static + Send,
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        let fut = context_future(self.context.clone(), future);
        match ::task::LazyExecutor.execute(fut) {
            Ok(()) => Ok(()),
            Err(err) => {
                let kind = err.kind();
                let mut future = err.into_future();
                Err(ExecuteError::new(
                    kind,
                    future.future.take().expect("future"),
                ))
            }
        }
    }
}

impl<T> Clone for ContextualExecutor<T> {
    fn clone(&self) -> Self {
        Self {
            context: self.context.clone(),
        }
    }
}

struct Context<'a>(&'a [*const fmt::Display]);

impl<'a> fmt::Display for Context<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.0.is_empty() {
            return Ok(());
        }

        for item in self.0 {
            // See `fn context()` for comments about this unsafe.
            let item = unsafe { &**item };
            write!(f, "{} ", item)?;
        }
        Ok(())
    }
}

/// Guards that the pushed context is removed from TLS afterwards.
///
/// Specifically, this protects even if the passed function panics,
/// as destructors are run while unwinding.
struct ContextGuard<'a>(&'a (fmt::Display + 'static));

impl<'a> ContextGuard<'a> {
    fn new(context: &'a (fmt::Display + 'static)) -> Self {
        // This is a raw pointer because of lifetime conflicts that require
        // the thread local to have a static lifetime.
        //
        // We don't want to require a static lifetime, and in fact,
        // only use the reference within this closure, so converting
        // to a raw pointer is safe.
        let raw = context as *const fmt::Display;
        CONTEXT.with(|ctxt| {
            ctxt.borrow_mut().push(raw);
        });
        ContextGuard(context)
    }
}

impl<'a> Drop for ContextGuard<'a> {
    fn drop(&mut self) {
        CONTEXT.with(|ctxt| {
            ctxt.borrow_mut().pop();
        });
    }
}

pub fn admin() -> Section {
    Section::Admin
}

#[derive(Copy, Clone, Debug)]
pub enum Section {
    Proxy,
    Admin,
}

/// A utility for logging actions taken on behalf of a server task.
#[derive(Clone, Debug)]
pub struct Server {
    section: Section,
    name: &'static str,
    listen: SocketAddr,
    remote: Option<SocketAddr>,
}

/// A utility for logging actions taken on behalf of a client task.
#[derive(Clone, Debug)]
pub struct Client<C: fmt::Display, D: fmt::Display> {
    section: Section,
    client: C,
    dst: D,
    settings: Option<::proxy::http::Settings>,
    remote: Option<SocketAddr>,
}

/// A utility for logging actions taken on behalf of a background task.
#[derive(Clone)]
pub struct Bg<T: fmt::Display = &'static str> {
    section: Section,
    name: T,
}

impl Section {
    pub fn bg<T: fmt::Display>(&self, name: T) -> Bg<T> {
        Bg {
            section: *self,
            name,
        }
    }

    pub fn server(&self, name: &'static str, listen: SocketAddr) -> Server {
        Server {
            section: *self,
            name,
            listen,
            remote: None,
        }
    }

    pub fn client<C: fmt::Display, D: fmt::Display>(&self, client: C, dst: D) -> Client<C, D> {
        Client {
            section: *self,
            client,
            dst,
            settings: None,
            remote: None,
        }
    }
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Section::Proxy => "proxy".fmt(f),
            Section::Admin => "admin".fmt(f),
        }
    }
}

pub type BgFuture<F, T> = ContextualFuture<Bg<T>, F>;
pub type ClientExecutor<C, D> = ContextualExecutor<Client<C, D>>;
pub type ServerExecutor = ContextualExecutor<Server>;
pub type ServerFuture<F> = ContextualFuture<Server, F>;

impl Server {
    pub fn proxy(name: &'static str, listen: SocketAddr) -> Self {
        Section::Proxy.server(name, listen)
    }

    pub fn with_remote(self, remote: SocketAddr) -> Self {
        Self {
            remote: Some(remote),
            ..self
        }
    }

    pub fn executor(self) -> ServerExecutor {
        context_executor(self)
    }

    pub fn future<F: Future>(self, f: F) -> ServerFuture<F> {
        context_future(self, f)
    }
}

impl fmt::Display for Server {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}={{server={} listen={}",
            self.section, self.name, self.listen
        )?;
        if let Some(remote) = self.remote {
            write!(f, " remote={}", remote)?;
        }
        write!(f, "}}")
    }
}

impl<D: fmt::Display> Client<&'static str, D> {
    pub fn proxy(name: &'static str, dst: D) -> Self {
        Section::Proxy.client(name, dst)
    }
}

impl<C: fmt::Display, D: fmt::Display> Client<C, D> {
    pub fn with_settings(self, p: ::proxy::http::Settings) -> Self {
        Self {
            settings: Some(p),
            ..self
        }
    }

    pub fn with_remote(self, remote: SocketAddr) -> Self {
        Self {
            remote: Some(remote),
            ..self
        }
    }

    pub fn executor(self) -> ClientExecutor<C, D> {
        context_executor(self)
    }
}

impl<C: fmt::Display, D: fmt::Display> fmt::Display for Client<C, D> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}={{client={} dst={}",
            self.section, self.client, self.dst
        )?;
        if let Some(ref proto) = self.settings {
            write!(f, " proto={:?}", proto)?;
        }
        if let Some(remote) = self.remote {
            write!(f, " remote={}", remote)?;
        }
        write!(f, "}}")
    }
}

impl<T: fmt::Display> Bg<T> {
    pub fn future<F: Future>(self, f: F) -> BgFuture<F, T> {
        context_future(self, f)
    }
}

impl<T: fmt::Display> fmt::Display for Bg<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}={{bg={}}}", self.section, self.name)
    }
}

// pub fn fmt_event<N>(ctx: &trace_fmt::span::Context<N>, f: &mut fmt::Write, event: &trace::Event) -> fmt::Result
// where
//     N: for<'a> trace_fmt::NewVisitor<'a>,
// {
//     let meta = event.metadata();
//     let level = match record.level() {
//         Level::TRACE => "TRCE",
//         Level::DEBUG => "DBUG",
//         Level::INFO => "INFO",
//         Level::WARN => "WARN",
//         Level::ERROR => "ERR!",
//     };
//     let uptime = clock::now() - start_time;
//     write!(
//         f,
//         "{} [{:>6}.{:06}s] {} {}",
//         level,
//         uptime.as_secs(),
//         uptime.subsec_micros(),
//         FmtCtx(&ctx),
//         meta.target()
//     )?;
//     {
//         let mut recorder = ctx.new_visitor(f, true);
//         event.record(&mut recorder);
//     }
//     ctx.with_current(|(_, span)| write!(f, " {}", span.fields()))
//         .unwrap_or(Ok(()))?;
//     writeln!(f)
// }

// struct FmtCtx<'a, N: 'a>(&'a trace_fmt::span::Context<'a, N>);

// impl<'a, N> fmt::Display for FmtCtx<'a, N> {
//     fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
//         let mut seen = false;
//         self.0.visit_spans(|_, span| {
//             write!(f, "{}", span.name())?;
//             seen = true;

//             let fields = span.fields();
//             if !fields.is_empty() {
//                 write!(f, "{{{}}}", fields)?;
//             }
//             ":".fmt(f)
//         })?;
//         if seen {
//             f.pad(" ")?;
//         }
//         Ok(())
//     }
// }
