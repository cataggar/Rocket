use std::fmt::Display;
use std::convert::TryInto;

use yansi::Paint;
use tokio::sync::Notify;

use crate::{Route, Catcher, Config, Shutdown};
use crate::router::Router;
use crate::fairing::{Fairing, Fairings};
use crate::logger::PaintExt;
use crate::shutdown::Shutdown;
use crate::http::{uri::Origin, ext::IntoOwned};
use crate::error::{Error, ErrorKind};

/// The main `Rocket` type: used to mount routes and catchers and launch the
/// application.
#[derive(Debug)]
pub struct Rocket {
    pub(crate) config: Config,
    pub(crate) figment: Figment,
    pub(crate) managed_state: Container![Send + Sync],
    pub(crate) router: Router,
    pub(crate) fairings: Fairings,
    pub(crate) shutdown_receiver: Option<mpsc::Receiver<()>>,
    pub(crate) shutdown_handle: Shutdown,
}

impl Rocket {
    /// Create a new `Rocket` application using the configuration information in
    /// `Rocket.toml`. If the file does not exist or if there is an I/O error
    /// reading the file, the defaults, overridden by any environment-based
    /// parameters, are used. See the [`config`](crate::config) documentation
    /// for more information on defaults.
    ///
    /// This method is typically called through the
    /// [`rocket::build()`](crate::build) alias.
    ///
    /// # Panics
    ///
    /// If there is an error reading configuration sources, this function prints
    /// a nice error message and then exits the process.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use rocket::launch;
    /// #[launch]
    /// fn rocket() -> _ {
    ///     rocket::build()
    /// }
    /// ```
    #[track_caller]
    #[inline(always)]
    pub fn build() -> Rocket {
        Rocket::custom(Config::figment())
    }

    /// Creates a new `Rocket` application using the supplied configuration
    /// provider. This method is typically called through the
    /// [`rocket::custom()`](crate::custom()) alias.
    ///
    /// # Panics
    ///
    /// If there is an error reading a [`Config`] from `provider`, function
    /// prints a nice error message and then exits the process.
    ///
    /// # Example
    ///
    /// ```rust
    /// use figment::{Figment, providers::{Toml, Env, Format}};
    ///
    /// #[rocket::launch]
    /// fn rocket() -> _ {
    ///     let figment = Figment::from(rocket::Config::default())
    ///         .merge(Toml::file("MyApp.toml").nested())
    ///         .merge(Env::prefixed("MY_APP_").global());
    ///
    ///     rocket::custom(figment)
    /// }
    /// ```
    #[track_caller]
    pub fn custom<T: figment::Provider>(provider: T) -> Rocket {
        let config = Config::from(&provider);
        let figment = Figment::from(provider);
        logger::init(&config);
        config.pretty_print(&figment);

        let managed_state = <Container![Send + Sync]>::new();
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(1);
        Rocket {
            config, figment, managed_state,
            shutdown_handle: Shutdown(shutdown_sender),
            router: Router::new(),
            fairings: Fairings::new(),
            shutdown_receiver: Some(shutdown_receiver),
        }
    }

    /// Resets the configuration in `self` to that provided by `provider`.
    ///
    /// # Panics
    ///
    /// Like [`Rocket::custom()`], panics if `provider` does not provide a valid
    /// [`Config`]. The error message is printed.
    ///
    /// # Examples
    ///
    /// To modify only some values, use the existing `config`:
    ///
    /// ```rust
    /// use std::net::Ipv4Addr;
    ///
    /// let config = rocket::Config {
    ///     port: 7777,
    ///     address: Ipv4Addr::new(18, 127, 0, 1).into(),
    ///     ..rocket::Config::default()
    /// };
    ///
    /// let rocket = rocket::custom(&config);
    /// assert_eq!(rocket.config().port, 7777);
    /// assert_eq!(rocket.config().address, Ipv4Addr::new(18, 127, 0, 1));
    ///
    /// // Modifying the existing config:
    /// let mut new_config = rocket.config().clone();
    /// new_config.port = 8888;
    ///
    /// // Note that this tosses away any non-`Config` parameters in `Figment`.
    /// let rocket = rocket.reconfigure(new_config);
    /// assert_eq!(rocket.config().port, 8888);
    /// assert_eq!(rocket.config().address, Ipv4Addr::new(18, 127, 0, 1));
    ///
    /// // Modifying the existing figment:
    /// let mut new_figment = rocket.figment().clone()
    ///     .merge(("address", "171.64.200.10"));
    ///
    /// let rocket = rocket.reconfigure(new_figment);
    /// assert_eq!(rocket.config().port, 8888);
    /// assert_eq!(rocket.config().address, Ipv4Addr::new(171, 64, 200, 10));
    /// ```
    #[inline]
    #[track_caller]
    pub fn reconfigure<T: figment::Provider>(mut self, provider: T) -> Rocket {
        self.config = Config::from(&provider);
        self.figment = Figment::from(provider);
        logger::init(&self.config);
        self.config.pretty_print(&self.figment);
        self
    }

    /// Mounts all of the routes in the supplied vector at the given `base`
    /// path. Mounting a route with path `path` at path `base` makes the route
    /// available at `base/path`.
    ///
    /// # Panics
    ///
    /// Panics if the `base` mount point is not a valid static path: a valid
    /// origin URI without dynamic parameters.
    ///
    /// Panics if any route's URI is not a valid origin URI. This kind of panic
    /// is guaranteed not to occur if the routes were generated using Rocket's
    /// code generation.
    ///
    /// # Examples
    ///
    /// Use the `routes!` macro to mount routes created using the code
    /// generation facilities. Requests to the `/hello/world` URI will be
    /// dispatched to the `hi` route.
    ///
    /// ```rust,no_run
    /// # #[macro_use] extern crate rocket;
    /// #
    /// #[get("/world")]
    /// fn hi() -> &'static str {
    ///     "Hello!"
    /// }
    ///
    /// #[launch]
    /// fn rocket() -> _ {
    ///     rocket::build().mount("/hello", routes![hi])
    /// }
    /// ```
    ///
    /// Manually create a route named `hi` at path `"/world"` mounted at base
    /// `"/hello"`. Requests to the `/hello/world` URI will be dispatched to the
    /// `hi` route.
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::{Request, Route, Data, route};
    /// use rocket::http::Method;
    ///
    /// fn hi<'r>(req: &'r Request, _: Data) -> route::BoxFuture<'r> {
    ///     route::Outcome::from(req, "Hello!").pin()
    /// }
    ///
    /// #[launch]
    /// fn rocket() -> _ {
    ///     let hi_route = Route::new(Method::Get, "/world", hi);
    ///     rocket::build().mount("/hello", vec![hi_route])
    /// }
    /// ```
    pub fn mount<'a, B, R>(mut self, base: B, routes: R) -> Self
        where B: TryInto<Origin<'a>> + Clone + Display,
              B::Error: Display,
              R: Into<Vec<Route>>
    {
        let base_uri = base.clone().try_into()
            .map(|origin| origin.into_owned())
            .unwrap_or_else(|e| {
                error!("Invalid route base: {}.", Paint::white(&base));
                panic!("Error: {}", e);
            });

        if base_uri.query().is_some() {
            error!("Mount point '{}' contains query string.", base);
            panic!("Invalid mount point.");
        }

        info!("{}{} {} {}",
              Paint::emoji("🛰  "),
              Paint::magenta("Mounting"),
              Paint::blue(&base_uri),
              Paint::magenta("routes:"));

        for route in routes.into() {
            let mounted_route = route.clone()
                .map_base(|old| format!("{}{}", base, old))
                .unwrap_or_else(|e| {
                    error_!("Route `{}` has a malformed URI.", route);
                    error_!("{}", e);
                    panic!("Invalid route URI.");
                });

            info_!("{}", mounted_route);
            self.router.add_route(mounted_route);
        }

        self
    }

    /// Registers all of the catchers in the supplied vector, scoped to `base`.
    ///
    /// # Panics
    ///
    /// Panics if `base` is not a valid static path: a valid origin URI without
    /// dynamic parameters.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Request;
    ///
    /// #[catch(500)]
    /// fn internal_error() -> &'static str {
    ///     "Whoops! Looks like we messed up."
    /// }
    ///
    /// #[catch(400)]
    /// fn not_found(req: &Request) -> String {
    ///     format!("I couldn't find '{}'. Try something else?", req.uri())
    /// }
    ///
    /// #[launch]
    /// fn rocket() -> _ {
    ///     rocket::build().register("/", catchers![internal_error, not_found])
    /// }
    /// ```
    pub fn register<'a, B, C>(mut self, base: B, catchers: C) -> Self
        where B: TryInto<Origin<'a>> + Clone + Display,
              B::Error: Display,
              C: Into<Vec<Catcher>>
    {
        info!("{}{} {} {}",
              Paint::emoji("👾 "),
              Paint::magenta("Registering"),
              Paint::blue(&base),
              Paint::magenta("catchers:"));

        for catcher in catchers.into() {
            let mounted_catcher = catcher.clone()
                .map_base(|old| format!("{}{}", base, old))
                .unwrap_or_else(|e| {
                    error_!("Catcher `{}` has a malformed URI.", catcher);
                    error_!("{}", e);
                    panic!("Invalid catcher URI.");
                });

            info_!("{}", mounted_catcher);
            self.router.add_catcher(mounted_catcher);
        }

        self
    }

    /// Add `state` to the state managed by this instance of Rocket.
    ///
    /// This method can be called any number of times as long as each call
    /// refers to a different `T`.
    ///
    /// Managed state can be retrieved by any request handler via the
    /// [`State`](crate::State) request guard. In particular, if a value of type `T`
    /// is managed by Rocket, adding `State<T>` to the list of arguments in a
    /// request handler instructs Rocket to retrieve the managed value.
    ///
    /// # Panics
    ///
    /// Panics if state of type `T` is already being managed.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # #[macro_use] extern crate rocket;
    /// use rocket::State;
    ///
    /// struct MyValue(usize);
    ///
    /// #[get("/")]
    /// fn index(state: State<MyValue>) -> String {
    ///     format!("The stateful value is: {}", state.0)
    /// }
    ///
    /// #[launch]
    /// fn rocket() -> _ {
    ///     rocket::build()
    ///         .mount("/", routes![index])
    ///         .manage(MyValue(10))
    /// }
    /// ```
    #[inline]
    pub fn manage<T: Send + Sync + 'static>(self, state: T) -> Self {
        let type_name = std::any::type_name::<T>();
        if !self.managed_state.set(state) {
            error!("State for type '{}' is already being managed!", type_name);
            panic!("Aborting due to duplicately managed state.");
        }

        self
    }

    /// Attaches a fairing to this instance of Rocket. No fairings are excuted.
    /// Fairings will be executed at their appropriate time.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Rocket;
    /// use rocket::fairing::AdHoc;
    ///
    /// #[launch]
    /// fn rocket() -> _ {
    ///     rocket::build()
    ///         .attach(AdHoc::on_liftoff("Liftoff Message", |_| Box::pin(async {
    ///             println!("We have liftoff!");
    ///         })))
    /// }
    /// ```
    pub fn attach<F: Fairing>(mut self, fairing: F) -> Self {
        self.fairings.add(Box::new(fairing));
        self
    }

    /// Returns the active configuration.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Rocket;
    /// use rocket::fairing::AdHoc;
    ///
    /// #[launch]
    /// fn rocket() -> _ {
    ///     rocket::build()
    ///         .attach(AdHoc::on_liftoff("Print Config", |rocket| Box::pin(async move {
    ///             println!("Rocket launch config: {:?}", rocket.config());
    ///         })))
    /// }
    /// ```
    #[inline(always)]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Returns the figment for configured provider.
    ///
    /// # Example
    ///
    /// ```rust
    /// let rocket = rocket::build();
    /// let figment = rocket.figment();
    ///
    /// let port: u16 = figment.extract_inner("port").unwrap();
    /// assert_eq!(port, rocket.config().port);
    /// ```
    #[inline(always)]
    pub fn figment(&self) -> &Figment {
        &self.figment
    }

    /// Returns an iterator over all of the routes mounted on this instance of
    /// Rocket. The order is unspecified.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Rocket;
    /// use rocket::fairing::AdHoc;
    ///
    /// #[get("/hello")]
    /// fn hello() -> &'static str {
    ///     "Hello, world!"
    /// }
    ///
    /// fn main() {
    ///     let mut rocket = rocket::build()
    ///         .mount("/", routes![hello])
    ///         .mount("/hi", routes![hello]);
    ///
    ///     for route in rocket.routes() {
    ///         match route.uri.base() {
    ///             "/" => assert_eq!(route.uri.path(), "/hello"),
    ///             "/hi" => assert_eq!(route.uri.path(), "/hi/hello"),
    ///             _ => unreachable!("only /hello, /hi/hello are expected")
    ///         }
    ///     }
    ///
    ///     assert_eq!(rocket.routes().count(), 2);
    /// }
    /// ```
    #[inline(always)]
    pub fn routes(&self) -> impl Iterator<Item = &Route> {
        self.router.routes()
    }

    /// Returns an iterator over all of the catchers registered on this instance
    /// of Rocket. The order is unspecified.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::Rocket;
    /// use rocket::fairing::AdHoc;
    ///
    /// #[catch(404)] fn not_found() -> &'static str { "Nothing here, sorry!" }
    /// #[catch(500)] fn just_500() -> &'static str { "Whoops!?" }
    /// #[catch(default)] fn some_default() -> &'static str { "Everything else." }
    ///
    /// fn main() {
    ///     let mut rocket = rocket::build()
    ///         .register("/", catchers![not_found, just_500, some_default]);
    ///
    ///     let mut codes: Vec<_> = rocket.catchers().map(|c| c.code).collect();
    ///     codes.sort();
    ///
    ///     assert_eq!(codes, vec![None, Some(404), Some(500)]);
    /// }
    /// ```
    #[inline(always)]
    pub fn catchers(&self) -> impl Iterator<Item = &Catcher> {
        self.router.catchers()
    }

    /// Returns `Some` of the managed state value for the type `T` if it is
    /// being managed by `self`. Otherwise, returns `None`.
    ///
    /// # Example
    ///
    /// ```rust
    /// #[derive(PartialEq, Debug)]
    /// struct MyState(&'static str);
    ///
    /// let rocket = rocket::build().manage(MyState("hello!"));
    /// assert_eq!(rocket.state::<MyState>(), Some(&MyState("hello!")));
    /// ```
    #[inline(always)]
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.managed_state.try_get()
    }

    /// Returns a handle which can be used to gracefully terminate this instance
    /// of Rocket. In routes, use the [`Shutdown`] request guard.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use std::{thread, time::Duration};
    /// # rocket::async_test(async {
    /// let mut rocket = rocket::build();
    /// let handle = rocket.shutdown();
    ///
    /// thread::spawn(move || {
    ///     thread::sleep(Duration::from_secs(10));
    ///     handle.shutdown();
    /// });
    ///
    /// // Shuts down after 10 seconds
    /// let shutdown_result = rocket.launch().await;
    /// assert!(shutdown_result.is_ok());
    /// # });
    /// ```
    #[inline(always)]
    pub fn shutdown(&self) -> Shutdown {
        self.shutdown_handle.clone()
    }

    // Perform "pre-launch" checks: verify:
    //     * there are no routing colisionns
    //     * there were no fairing failures
    //     * a secret key, if needed, is securely configured
    pub async fn _ignite(mut self) -> Result<Rocket, Error> {
        // Check for routing collisions.
        if let Err(collisions) = self.router.finalize() {
            return Err(Error::new(ErrorKind::Collisions(collisions)));
        }

        // Check for safely configured secrets.
        #[cfg(feature = "secrets")]
        if !self.config.secret_key.is_provided() {
            let profile = self.figment.profile();
            if profile != Config::DEBUG_PROFILE {
                return Err(Error::new(ErrorKind::InsecureSecretKey(profile.clone())));
            } else if self.config.secret_key.is_zero() {
                self.config.secret_key = crate::config::SecretKey::generate()
                    .unwrap_or(crate::config::SecretKey::zero());

                warn!("secrets enabled without a stable `secret_key`");
                info_!("disable `secrets` feature or configure a `secret_key`");
                info_!("this becomes an {} in non-debug profiles", Paint::red("error"));

                if !self.config.secret_key.is_zero() {
                    warn_!("a random key has been generated for this launch");
                }
            }
        };

        // Run launch fairings. Check for failed fairings.
        self = Fairings::handle_launch(self).await;
        if let Some(failures) = self.fairings.failures() {
            return Err(Error::new(ErrorKind::FailedFairings(failures.to_vec())))
        }

        // Freeze managed state for synchronization-free accesses later.
        self.managed_state.freeze();

        // Show all of the fairings.
        self.fairings.pretty_print_counts();

        Ok(self)
    }

    /// Returns a `Future` that drives the server, listening for and dispatching
    /// requests to mounted routes and catchers. The `Future` completes when the
    /// server is shut down via [`Shutdown`], encounters a fatal error, or if
    /// the the `ctrlc` configuration option is set, when `Ctrl+C` is pressed.
    ///
    /// # Error
    ///
    /// If there is a problem starting the application, an [`Error`] is
    /// returned. Note that a value of type `Error` panics if dropped without
    /// first being inspected. See the [`Error`] documentation for more
    /// information.
    ///
    /// # Example
    ///
    /// ```rust
    /// #[rocket::main]
    /// async fn main() {
    /// # if false {
    ///     let result = rocket::build().launch().await;
    ///     assert!(result.is_ok());
    /// # }
    /// }
    /// ```
    pub async fn launch(self) -> Result<(), Error> {
        let rocket = self._ignite().await?;

        rocket.default_tcp_http_server(|rocket| Box::pin(async move {
            let proto = rocket.config.tls_enabled().then(|| "https").unwrap_or("http");
            let addr = format!("{}://{}:{}", proto, rocket.config.address, rocket.config.port);
            launch_info!("{}{} {}",
                Paint::emoji("🚀 "),
                Paint::default("Rocket has launched from").bold(),
                Paint::default(addr).bold().underline());

            rocket.fairings.handle_liftoff(&rocket).await;
        })).await
    }
}
