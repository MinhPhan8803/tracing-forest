//! Run asynchronous code in the context of a `tracing-forest` subscriber.
//! 
//! This module provides two ways to execute async code: [`new`] for `main` functions,
//! and [`capture`] for unit tests.
//! 
//! # Nonblocking log processing with `new`
//! 
//! `tracing-forest` collects trace data into trees, and can sometimes
//! produce large trees that need to be processed. To avoid blocking the working
//! task in these cases, a common strategy is to send this data to a processing
//! task for formatting and writing.
//! 
//! The [`new`] function provides this behavior as a first-class feature of this
//! crate, and handles the configuration, initialization, and graceful shutdown
//! of a subscriber with an associated processing task.
//! 
//! ## Examples
//! 
//! ```
//! # use crate::tracing_forest::processor::Processor;
//! #[tokio::main]
//! async fn main() {
//!     tracing_forest::new()
//!         .map_sender(|sender| sender.with_stderr_fallback())
//!         .on_registry()
//!         .on(async {
//!             info!("Hello, world!");
//!
//!             info_span!("my_span").in_scope(|| {
//!                 info!("Relevant information");
//!             })
//!         })
//!         .await;
//! }
//! ```
//! ```log
//! INFO     💬 [info]: Hello, world!
//! INFO     my_span [ 26.0µs | 100.000% ]
//! INFO     ┕━ 💬 [info]: Relevant information
//! ```
//! 
//! # Inspecting trace data in unit tests with `capture`
//! 
//! Automated testing and reproducibility are critical to systems, and the [`capture`]
//! function offers the ability to programmatically inspect log trees generated by
//! `tracing-forest`. It is the unit testing analog of [`new`], except it returns
//! `Vec<Tree>` after the future is completed, which can be inspected using the
//! [`Tree`] public API.
//! 
//! ## Examples
//! 
//! ```
//! # use tracing::{error, info, info_span, trace};
//! #[tokio::test]
//! async fn my_test() -> Result<(), Box<dyn std::error::Error>> {
//!     let logs = tracing_forest::capture()
//!         .on_registry()
//!         .on(async {
//!             info!("Hello, world!");
//!
//!             info_span!("my_span").in_scope(|| {
//!                 info!("Relevant information");
//!             })
//!         })
//!         .await;
//!     
//!     // There is one event and one span
//!     assert!(logs.len() == 2);
//!     
//!     // Inspect the first event
//!     let hello = logs[0].event()?;
//!     assert!(hello.message() == "Hello, world!");
//!
//!     // Inspect the span
//!     let span = logs[1].span()?;
//!     assert!(span.name() == "my_span");
//! 
//!     // Only the `info` event is recorded
//!     assert!(span.children().len() == 1)
//! 
//!     let info = span.children()[0].event()?;
//! 
//!     assert!(info.message() == "Relevant information");
//! 
//!     Ok(())
//! }
//! ```
//! 
//! # Configuring `tracing-forest` subscribers
//! 
//! Both [`new`] and [`capture`] use the [builder pattern][builder] to configure
//! a subscriber. This happens in two stages: building the `TreeLayer`, and
//! building the `Subscriber`.
//! 
//! Start by calling either [`new`] or [`capture`] to get a [`LayerBuilder`], which
//! is responsible for configuring the internal `TreeLayer`. Options include
//! [setting the tag][set_tag], setting if the subscriber should be [set globally][set_global],
//! and configuring the processors [within the subscriber][map_sender] and the
//! [processing task][map_receiver].
//! 
//! Next, the [`on_registry`] method returns a [`SubscriberBuilder`] by constructing
//! the [`TreeLayer`] and composing it onto a [`Registry`]. This is a shortcut for
//! the more generalized [`on_subscriber`] method. The [`SubscriberBuilder`] provides
//! the [`with`] method, allowing for other [`Layer`]s and [`Filter`]s to be stacked
//! on top of the [`TreeLayer`].
//! 
//! Once finished, the configured subscriber can be used on a `Future` with the
//! [`on`] function. In the case of [`new`], `on` returns a `Future` resolving to
//! the unit type on completion. In the case of [`capture`], then `on` returns a
//! `Future` resolving to `Vec<Tree>`, which can then be inspected.
//! 
//! [builder]: https://rust-lang.github.io/api-guidelines/type-safety.html#builders-enable-construction-of-complex-values-c-builder
//! [set_tag]: LayerBuilder::set_tag
//! [set_global]: LayerBuilder::set_global
//! [map_sender]: LayerBuilder::map_sender
//! [map_receiver]: LayerBuilder::map_receiver
//! [`on_registry`]: LayerBuilder::on_registry
//! [`on_subscriber`]: LayerBuilder::on_subscriber
//! [`with`]: SubscriberBuilder::with
//! [`Filter`]: tracing_subscriber::layer::Filter
//! [`on`]: SubscriberBuilder::on
use crate::formatter::Pretty;
use crate::layer::Tree;
use crate::processor::{Printer, Processor, WithFallback};
use crate::sealed::Sealed;
use crate::tag::{NoTag, Tag, TagParser};
use crate::{fail, TreeLayer};
use tracing::Subscriber;
use tracing_subscriber::layer::Layered;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry, EnvFilter};
use std::future::Future;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

pub(crate) type MakeStdout = fn() -> std::io::Stdout;

/// Returns a [`LayerBuilder`] that will send log trees to a processing task.
/// 
/// See the [module-level documentation][nonblocking-processing] for more details.
/// 
/// # Note
/// 
/// The [`new`] function defaults to setting the global subscriber, which is required
/// to detect logs in multithreading scenarios, but prevents setting other [`Subscriber`]s
/// globally afterwards. This can be disabled via the [`set_global`][set_global]
/// method.
/// 
/// [nonblocking-processing]: crate::builder#nonblocking-log-processing-with-new
pub fn new() -> LayerBuilder<TreeSender, Process<Printer<Pretty, MakeStdout>>> {
    let (sender_processor, receiver) = mpsc::unbounded_channel();
    let receiver_processor = Process(Printer::new(Pretty::new(), std::io::stdout as _));

    LayerBuilder {
        sender_processor: TreeSender(sender_processor),
        receiver_processor,
        receiver,
        tag: NoTag::from_field,
        is_global: true,
    }
}

/// Returns a [`LayerBuilder`] that will store log trees for later processing.
/// 
/// See the [module-level documentation][inspecting-trace-data] for more details.
/// 
/// # Note
/// 
/// The [`capture`] function defaults to not setting the global subscriber, which
/// allows multiple unit tests in the same file, but prevents trace data from other
/// threads to be collected. This can be enabled via the [`set_global`][set_global]
/// method.
/// 
/// [inspecting-trace-data]: crate::builder#inspecting-trace-data-in-unit-tests-with-capture
pub fn capture() -> LayerBuilder<TreeSender, Capture> {
    let (sender_processor, receiver) = mpsc::unbounded_channel();

    LayerBuilder {
        sender_processor: TreeSender(sender_processor),
        receiver_processor: Capture(()),
        receiver,
        tag: NoTag::from_field,
        is_global: false,
    }
}


/// Configures and constructs [`SubscriberBuilder`]s.
///
/// This type is returned from [`new`] and [`capture`]. 
pub struct LayerBuilder<T: Processor, R> {
    sender_processor: T,
    receiver_processor: R,
    receiver: UnboundedReceiver<Tree>,
    tag: TagParser,
    is_global: bool,
}

/// A marker type indicating that trace data should be captured for later use.
pub struct Capture(());

/// A marker type indicating that trace data should be processed.
pub struct Process<P: Processor>(P);

/// The [`Processor`] used within a `tracing-forest` subscriber for sending logs
/// to a processing task.
pub struct TreeSender(UnboundedSender<Tree>);

impl Processor for TreeSender {
    fn process(&self, tree: Tree) -> Result<(), crate::processor::ProcessingError> {
        self.0.process(tree)
    }
}

#[doc(hidden)]
pub trait SealedSender: Sealed {}

impl Sealed for TreeSender {}
impl SealedSender for TreeSender {}

impl<S: SealedSender, P> Sealed for WithFallback<S, P> {}
impl<S: SealedSender, P> SealedSender for WithFallback<S, P> {}

impl<T, R> LayerBuilder<T, Process<R>>
where
    T: Processor,
    R: Processor,
{
    /// Configure the processor on the receiving end of the log channel.
    /// This is particularly useful for adding fallbacks.
    /// 
    /// # Examples
    ///
    /// Updating the receiver in a [`LayerBuilder`] generated by [`new`].
    /// ```
    /// # use crate::tracing_forest::processor::Processor;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// tracing_forest::new()
    ///     .map_receiver(|receiver| {
    ///         receiver
    ///             .with_writer(std::io::stderr)
    ///             .with_stderr_fallback()
    ///     })
    ///     .on_registry()
    ///     .on(async {
    ///         // ...
    ///     })
    ///     .await;
    /// # }
    /// ```
    pub fn map_receiver<F, R2>(self, f: F) -> LayerBuilder<T, Process<R2>>
    where
        F: FnOnce(R) -> R2,
        R2: Processor,
    {
        LayerBuilder {
            sender_processor: self.sender_processor,
            receiver_processor: Process(f(self.receiver_processor.0)),
            receiver: self.receiver,
            tag: self.tag,
            is_global: self.is_global,
        }
    }
}

impl<T, R> LayerBuilder<T, R>
where
    T: Processor + SealedSender,
{
    /// Configure the processer within the subscriber that sends log trees to
    /// a processing task.
    /// 
    /// # Examples
    ///
    /// Updating the sender in a [`LayerBuilder`].
    /// ```
    /// # use std::fs::File;
    /// # use crate::tracing_forest::processor::Processor;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// tracing_forest::new()
    ///     .map_sender(|sender| sender.with_stderr_fallback())
    ///     .on_registry()
    ///     .on(async {
    ///         // ...
    ///     })
    ///     .await;
    /// # }
    /// ```
    /// 
    /// Note that some wrapping of the existing sender must be returned. Returning
    /// a different [`Processor`] will result in a compilation error.
    /// ```compile_fail
    /// # use std::fs::File;
    /// # use tracing_forest::processor::Printer;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// tracing_forest::new()
    ///     .map_sender(|_sender| Printer::default())
    ///     .on_registry()
    ///     .on(async {
    ///         // ...
    ///     })
    ///     .await;
    /// # }
    /// ```
    pub fn map_sender<F, T2>(self, f: F) -> LayerBuilder<T2, R>
    where
        F: FnOnce(T) -> T2,
        T2: Processor + SealedSender,
    {
        LayerBuilder {
            sender_processor: f(self.sender_processor),
            receiver_processor: self.receiver_processor,
            receiver: self.receiver,
            tag: self.tag,
            is_global: self.is_global,

        }
    }

    /// Set the tag parser.
    pub fn set_tag(mut self, tag: TagParser) -> Self {
        self.tag = tag;
        self
    }

    /// Set whether or not the subscriber should be set globally.
    pub fn set_global(mut self, is_global: bool) -> Self {
        self.is_global = is_global;
        self
    }

    /// Finish building the [`TreeLayer`], and compose it onto the provided [`Subscriber`].
    pub fn on_subscriber<S>(
        self,
        subscriber: S,
    ) -> SubscriberBuilder<Layered<TreeLayer<T>, S>, R>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let subscriber = TreeLayer::new(self.sender_processor)
            .set_tag(self.tag)
            .with_subscriber(subscriber);

        SubscriberBuilder {
            subscriber,
            output: self.receiver_processor,
            receiver: self.receiver,
            is_global: self.is_global,
        }
    }

    /// Finish building the [`TreeLayer`], and compose it onto a [`Registry`].
    pub fn on_registry(
        self,
    ) -> SubscriberBuilder<Layered<TreeLayer<T>, Registry>, R> {
        self.on_subscriber(Registry::default())
    }
}

/// Configures a `tracing-forest` subscriber to run in the context of a `Future`.
/// 
/// This type is returned by [`on_registry`][LayerBuilder::on_registry] and
/// [`on_subscriber`][LayerBuilder::on_subscriber].
pub struct SubscriberBuilder<S, O> {
    subscriber: S,
    output: O,
    receiver: UnboundedReceiver<Tree>,
    is_global: bool,
}

impl<S, O> SubscriberBuilder<S, O>
where
    S: Subscriber,
{
    /// Wraps the inner subscriber with the provided [`Layer`].
    ///
    /// # Examples
    /// ```
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// tracing_forest::new()
    ///     .on_registry()
    ///     .with(tracing_subscriber::filter::LevelFilter::WARN)
    ///     .on(async {
    ///         // do stuff here...
    ///     })
    ///     .await;
    /// # }
    /// ```
    pub fn with<L>(self, layer: L) -> SubscriberBuilder<Layered<L, S>, O>
    where
        L: Layer<S>,
    {
        SubscriberBuilder {
            subscriber: layer.with_subscriber(self.subscriber),
            output: self.output,
            receiver: self.receiver,
            is_global: self.is_global,
        }
    }

    /// Wraps the inner subscriber with the default [`EnvFilter`].
    pub fn with_env_filter(self) -> SubscriberBuilder<Layered<EnvFilter, S>, O> {
        self.with(EnvFilter::from_default_env())
    }
}

impl<S, P> SubscriberBuilder<S, Process<P>>
where
    S: Subscriber + Send + Sync,
    P: Processor + Send,
{
    /// Execute a future in the context of the configured subscriber.
    pub async fn on(self, f: impl Future<Output = ()>) {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let processor = self.output.0;
        let mut receiver = self.receiver;

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(tree) = receiver.recv() => {
                        processor.process(tree).unwrap_or_else(fail::processing_error);
                    }
                    Ok(()) = &mut shutdown_rx => {
                        receiver.close();
                        break;
                    }
                }
            }

            // Drain any remaining logs in the channel buffer.
            while let Some(tree) = receiver.recv().await {
                processor.process(tree).unwrap_or_else(fail::processing_error);
            }
        });

        if self.is_global {
            tracing::subscriber::set_global_default(self.subscriber)
                .expect("global default already set");
            f.await;
        } else {
            let _guard = tracing::subscriber::set_default(self.subscriber);
            f.await;
        }

        shutdown_tx.send(()).expect("Shutdown signal couldn't send, this is a bug.");

        handle.await.expect("Failed to join the writing task, this is a bug.");
    }
}

impl<S> SubscriberBuilder<S, Capture>
where
    S: Subscriber + Send + Sync,
{
    /// Execute a future in the context of the configured subscriber, and return
    /// a `Vec<Tree>` of generated logs.
    pub async fn on(mut self, f: impl Future<Output = ()>) -> Vec<Tree> {
        if self.is_global {
            tracing::subscriber::set_global_default(self.subscriber)
                .expect("global default already set");
            f.await;
        } else {
            let _guard = tracing::subscriber::set_default(self.subscriber);
            f.await;
        }

        self.receiver.close();

        let mut logs = Vec::with_capacity(0);

        while let Some(tree) = self.receiver.recv().await {
            logs.push(tree);
        }

        logs
    }
}
