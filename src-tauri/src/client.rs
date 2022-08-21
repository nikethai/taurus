use crate::user_agent::{random_user_agent, UserAgent};
use reqwest::{Client, Result, redirect::Policy};
use std::time::Duration;

// Taken from agourlay/dlm library
pub fn make_client(
    user_agent: Option<UserAgent>,
    redirect: Option<bool>,
) -> Result<Client> {
    let client_builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(0);

    // User agent
    let client_builder = match user_agent {
        Some(UserAgent::CustomUserAgent(ua)) => client_builder.user_agent(ua),
        Some(UserAgent::RandomUserAgent) => client_builder.user_agent(random_user_agent()),
        _ => client_builder,
    };

    let redirect = match redirect {
        Some(val) => val,
        None => true,
    };

    // setup redirect
    let client_builder = if redirect {
        // defaults to 10 redirects
        client_builder.redirect(Policy::default())
    } else {
        client_builder.redirect(Policy::none())
    };

    client_builder.build()
}
