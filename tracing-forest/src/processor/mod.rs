//! Trait for processing logs of a span after it is closed.
//!
//! See [`Processor`] for more details.
use crate::cfg_tokio;
use crate::printer::{Printer, StderrPrinter, StdoutPrinter};
use crate::tree::Tree;
use std::io;
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::Arc;

mod error;
pub use error::ProcessReport;
use error::SendError;

/// A type that can process [trace trees].
///
/// `Processor`s are responsible for both formatting and writing logs to their
/// intended destinations. This is typically implemented using
///
/// [`Formatter`], [`MakeWriter`], and [`io::Write`].
///
/// [trace trees]: crate::tree::Tree
/// [`Formatter`]: crate::printer::Formatter
/// [`MakeWriter`]: tracing_subscriber::fmt::MakeWriter
pub trait Processor: 'static + Sized {
    /// Processes the [`Tree`] of logs. This can mean many things, such as writing
    /// to stdout or a file, sending over a network, storing in memory, ignoring,
    /// or anything else.
    ///
    /// # Errors
    ///
    /// If the `Tree` cannot be processed, then a [`ProcessReport`] is returned
    /// which may or may not be recoverable with a [fallback].
    ///
    /// [fallback]: Processor::with_fallback
    fn process(&self, tree: Tree) -> Result<(), ProcessReport>;

    /// Returns a `Processor` that first attempts processing with `self`, and
    /// resorts to processing with `fallback` on failure.
    fn with_fallback<P>(self, fallback: P) -> WithFallback<Self, P>
    where
        P: Processor,
    {
        WithFallback {
            primary: self,
            fallback,
        }
    }

    /// Returns a `Processor` that first attempts processing with `self`, and
    /// resorts to pretty-printing to stdout on failure.
    fn with_stdout_fallback(self) -> WithFallback<Self, StdoutPrinter> {
        let fallback = Printer::from_make_writer(io::stdout as _);
        self.with_fallback(fallback)
    }

    /// Returns a `Processor` that first attempts processing with `self`, and
    /// resorts to pretty-printing to stderr on failure.
    fn with_stderr_fallback(self) -> WithFallback<Self, StderrPrinter> {
        let fallback = Printer::from_make_writer(io::stderr as _);
        self.with_fallback(fallback)
    }

    /// Returns a `Processor` that silently fails if `self` fails to process.
    fn with_ignore_fallback(self) -> WithFallback<Self, Sink> {
        self.with_fallback(Sink)
    }
}

/// A [`Processor`] processor composed of a primary and a fallback `Processor`.
///
/// This type is returned by [`Processor::with_fallback`].
#[derive(Debug)]
pub struct WithFallback<P, F> {
    primary: P,
    fallback: F,
}

impl<P, F> Processor for WithFallback<P, F>
where
    P: Processor,
    F: Processor,
{
    fn process(&self, tree: Tree) -> Result<(), ProcessReport> {
        self.primary
            .process(tree)
            .or_else(|err| err.try_fallback(&self.fallback))
    }
}

/// A [`Processor`] that ignores any incoming logs.
///
/// This processor cannot fail.
#[derive(Debug)]
pub struct Sink;

impl Processor for Sink {
    fn process(&self, _tree: Tree) -> Result<(), ProcessReport> {
        Ok(())
    }
}

impl<P: Processor> Processor for Box<P> {
    fn process(&self, tree: Tree) -> Result<(), ProcessReport> {
        self.as_ref().process(tree)
    }
}

impl<P: Processor> Processor for Arc<P> {
    fn process(&self, tree: Tree) -> Result<(), ProcessReport> {
        self.as_ref().process(tree)
    }
}

impl Processor for Sender<Tree> {
    fn process(&self, tree: Tree) -> Result<(), ProcessReport> {
        self.send(tree)
            .map_err(|err| ProcessReport::new(Some(err.0), SendError.into()))
    }
}

impl Processor for SyncSender<Tree> {
    fn process(&self, tree: Tree) -> Result<(), ProcessReport> {
        self.send(tree)
            .map_err(|err| ProcessReport::new(Some(err.0), SendError.into()))
    }
}

cfg_tokio! {
    use tokio::sync::mpsc::UnboundedSender;

    impl Processor for UnboundedSender<Tree> {
        fn process(&self, tree: Tree) -> Result<(), ProcessReport> {
            self.send(tree)
                .map_err(|err| ProcessReport::new(Some(err.0), SendError.into()))
        }
    }
}
