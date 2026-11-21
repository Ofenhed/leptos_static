//! This library can be used to generate a static version of a leptos website. If you don't have
//! any server side functionality, this may allow you to host your website on a static host.
//!
//! Example:
//!
//! ```no_run
//! # use leptos_static::prelude::*;
//! # use std::pin::pin;
//! async fn generate_static() {
//! # let leptos_options: leptos::prelude::LeptosOptions = leptos::prelude::LeptosOptions::builder().output_name("test").build();
//! # let shell = |_| ();
//!     let app = {
//!         let leptos_options = leptos_options.clone();
//!         move || shell(leptos_options.clone())
//!     };
//!     let mut routes = generate_static_files_list(app.clone(), || ())
//!         .await
//!         .unwrap();
//!     add_404(&mut routes, &leptos_options);
//!     let generator = StaticRouteGenerator::new(&routes, app, || ());
//!     generator.generate(&leptos_options).await;
//! }
//! ```
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

use any_spawner::PinnedFuture;
use futures::{StreamExt as _, join};
use leptos::{config::LeptosOptions, prelude::*};
use leptos_integration_utils::{BoxedFnOnce, PinnedStream};
use leptos_router::{
    Method, PathSegment, RouteList, RouteListing, SsrMode, static_routes::StaticRoute,
};

#[derive(Clone)]
struct NoGenerateStatic(WriteSignal<bool>);

/// Let the current [`StaticRouteGenerator`] know that you don't want the page we're currently
/// rendering should not be saved to a static file, despite the [`ssr_mode`] indicating that it
/// should. This does not abort the rendering, and can later be overwritten by a subsequent call to
/// [`set_skip_static_file_generation()`].
///
/// [`ssr_mode`]: leptos_router::NestedRoute::ssr_mode
pub fn set_skip_static_file_generation(skip: bool) {
    let NoGenerateStatic(signal) = use_context()
        .expect("Tried to call set_skip_static_file_generation outside of StaticRouteGenerator");
    *signal.write_untracked() = skip;
}

#[derive(thiserror::Error, Debug)]
pub enum StaticFileGeneratorError {
    #[error("Executor error: {0}")]
    Executor(#[from] any_spawner::ExecutorError),
    #[error("Invalid path: {}", .0.display())]
    InvalidPath(Oco<'static, Path>),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("Could not generate routes")]
    NoRoutesGenerated,
    #[error("Duplicate paths: {}", .0.join(" "))]
    DuplicatePath(Vec<String>),
}

fn init_executor() -> Result<(), any_spawner::ExecutorError> {
    #[cfg(feature = "tokio")]
    {
        any_spawner::Executor::init_tokio()
    }
    #[cfg(feature = "i-initiated-my-own-executor")]
    {
        Ok(())
    }
    #[cfg(all(not(feature = "i-initiated-my-own-executor"), not(feature = "tokio")))]
    {
        compile_error!(
            "No supported executor chosen. If you have your own executor, it has to be initiated before calling generate_static_files_list. If so, add the `i-initiated-my-own-executor` to the feature list of leptos_static"
        )
    }
}

fn static_path(
    leptos: &LeptosOptions,
    path: &str,
) -> Result<Oco<'static, Path>, StaticFileGeneratorError> {
    let mut p = format!("./{}", path);
    if p.ends_with("/") {
        p.push_str("index");
    }
    p.push_str(".html");
    let p = PathBuf::from(p);
    if p.components()
        .find(|x| {
            use std::path::Component as C;
            !matches!(x, C::Normal(_) | C::CurDir)
        })
        .is_some()
    {
        return Err(StaticFileGeneratorError::InvalidPath(Oco::Owned(p)));
    }
    let root = Path::new(&*leptos.site_root);
    let mut result = root.to_path_buf();
    result.push(p);
    if !result.starts_with(root) {
        return Err(StaticFileGeneratorError::InvalidPath(Oco::Owned(result)));
    }
    Ok(Oco::Owned(result))
}

pub type StaticRouteGeneratorFunction = Box<dyn FnOnce(&LeptosOptions) -> PinnedFuture<()> + Send>;
pub struct StaticRouteGenerator(#[allow(unused)] Owner, StaticRouteGeneratorFunction);

#[derive(Clone)]
struct NoGenerateStaticReader(ReadSignal<bool>);

fn async_stream_builder<IV>(
    app: IV,
    chunks: BoxedFnOnce<PinnedStream<String>>,
    supports_ooo: bool,
) -> PinnedFuture<PinnedStream<String>>
where
    IV: IntoView + 'static,
{
    Box::pin(async move {
        let app = if supports_ooo {
            app.to_html_stream_out_of_order()
        } else {
            app.to_html_stream_in_order()
        };
        let app = app.collect::<String>().await;
        let chunks = chunks();
        Box::pin(futures::stream::once(async move { app }).chain(chunks)) as PinnedStream<String>
    })
}

impl StaticRouteGenerator {
    /// Render a route and return the HTML code. This does not consider the [`ssr_mode`] of the
    /// route.
    ///
    /// [`ssr_mode`]: leptos_router::NestedRoute::ssr_mode
    pub fn render_route<IV: IntoView + 'static>(
        path: Oco<'static, str>,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
        additional_context: impl Fn() + Clone + Send + 'static,
    ) -> impl Future<Output = (Owner, String)> {
        #[cfg(feature = "meta")]
        let (meta_context, meta_output) = leptos_meta::ServerMetaContext::new();
        let additional_context = {
            move || {
                provide_context(leptos_router::location::RequestUrl::new(path.as_ref()));
                #[cfg(feature = "meta")]
                {
                    provide_context(meta_context);
                }
                additional_context();
            }
        };
        let (owner, stream) = leptos_integration_utils::build_response(
            app_fn.clone(),
            additional_context,
            async_stream_builder,
            false,
        );

        let sc = owner.shared_context().unwrap();

        async move {
            let await_pending = async move {
                while let Some(pending) = sc.await_deferred() {
                    pending.await;
                }
            };
            let (stream, _) = join!(stream, await_pending);

            #[cfg(feature = "meta")]
            let stream = meta_output.inject_meta_context(stream).await;
            let html = stream.collect::<String>().await;
            (owner, html)
        }
    }

    pub fn new<IV>(
        routes: &RouteList,
        app_fn: impl Fn() -> IV + Clone + Send + 'static,
        additional_context: impl Fn() + Clone + Send + 'static,
    ) -> Self
    where
        IV: IntoView + 'static,
    {
        let owner = Owner::new();
        Self(owner.clone(), {
            let routes = routes.clone();
            Box::new(move |options| {
                let options = options.clone();
                let app_fn = app_fn.clone();
                let additional_context = additional_context.clone();
                let additional_context = move || {
                    let (read_ignored, write_ignored) = signal(false);
                    provide_context(NoGenerateStatic(write_ignored));
                    provide_context(NoGenerateStaticReader(read_ignored));
                    additional_context();
                };

                owner.with(|| {
                    Box::pin(ScopedFuture::new(routes.generate_static_files(
                        move |path| {
                            #[cfg(feature = "tracing")]
                            let _ = tracing::span!(
                                tracing::Level::TRACE,
                                "Requesting render of {path}"
                            );
                            Self::render_route(
                                Oco::Owned(path.to_string()),
                                app_fn.clone(),
                                additional_context.clone(),
                            )
                        },
                        move |path, owner, content| {
                            #[cfg(feature = "tracing")]
                            let path = path.to_owned();
                            let target =
                                static_path(&options, path.as_ref()).map_err(std::io::Error::other);
                            #[cfg(feature = "tracing")]
                            let trace = tracing::span!(tracing::Level::TRACE, "Rendering {path}");
                            let dont_generate = owner.with(|| {
                                let NoGenerateStaticReader(r) = use_context()
                                    .expect("State from StaticRouteGenerator has been corrupted");
                                r.get()
                            });

                            async move {
                                if dont_generate {
                                    #[cfg(feature = "tracing")]
                                    tracing::event!(
                                        parent: &trace,
                                        tracing::Level::TRACE,
                                        "Ignoring path {path}"
                                    );
                                    return Ok(());
                                }
                                let target = target?;
                                #[cfg(feature = "tracing")]
                                tracing::event!(
                                    parent: &trace,
                                    tracing::Level::TRACE,
                                    "Saving {path} as {}",
                                    target.display()
                                );
                                if let Some(parent) = target.parent() {
                                    #[cfg(feature = "tokio")]
                                    tokio::fs::create_dir_all(parent).await?;
                                    #[cfg(not(feature = "tokio"))]
                                    std::fs::create_dir_all(parent)?;
                                }
                                #[cfg(feature = "tokio")]
                                tokio::fs::write(target, content).await?;
                                #[cfg(not(feature = "tokio"))]
                                std::fs::write(target, content)?;
                                #[cfg(feature = "tracing")]
                                tracing::event!(parent: &trace, tracing::Level::TRACE, "Done");
                                Ok(())
                            }
                        },
                        |_owner| false,
                    )))
                })
            })
        })
    }

    /// Run your app for each path and save the output files.
    pub async fn generate(self, options: &LeptosOptions) {
        (self.1)(options).await
    }
}

/// Add the 404 path defined in the [leptos options] to the list of files to be rendered and
/// saved. The path sent to the [router] will be the same as the output path (without the
/// filetype suffix).
///
/// [leptos options]: leptos::prelude::LeptosOptions::not_found_path
/// [router]: leptos_router
pub fn add_404(list: &mut RouteList, options: &LeptosOptions) {
    let not_found_path = options.not_found_path.clone();
    let route_404 = RouteListing::new(
        [PathSegment::Static(Cow::Owned(not_found_path.to_string()))],
        SsrMode::Static(StaticRoute::new()),
        [Method::Get],
        [],
    );
    if list.iter().find(|x| x.path() == route_404.path()).is_none() {
        list.push(route_404);
    }
}

/// Generate a list of all files that should be rendered to be used as input to
/// [`StaticRouteGenerator::new()`].
pub async fn generate_static_files_list<IV: IntoView + 'static>(
    app_fn: impl Clone + Send + 'static + Fn() -> IV,
    additional_context: impl Clone + Send + 'static + Fn(),
) -> Result<RouteList, StaticFileGeneratorError> {
    init_executor()?;
    let _owner = Owner::new_root(Some(Arc::from(hydration_context::SsrSharedContext::new())));
    provide_context(leptos_router::location::RequestUrl::new(""));
    #[cfg(feature = "meta")]
    {
        use leptos_meta::*;
        let (mock_meta, _) = ServerMetaContext::new();
        provide_context(mock_meta);
    }
    let (_, mock_writer) = signal(false);
    provide_context(NoGenerateStatic(mock_writer));
    additional_context();
    leptos_router::RouteList::generate(app_fn).ok_or(StaticFileGeneratorError::NoRoutesGenerated)
}
