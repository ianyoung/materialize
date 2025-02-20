// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;

use anyhow::{bail, Context, Ok, Result};
use axum::http::StatusCode;
use axum::{extract::Query, response::IntoResponse, routing::get, Router};
use reqwest::Client;
use tokio::sync::broadcast::{channel, Sender};

use crate::configuration::{Endpoint, FronteggAPIToken, FronteggAuth};
use crate::utils::{trim_newline, RequestBuilderExt};

use crate::BrowserAPIToken;

/// Request handler for the server waiting the browser API token creation
// Axum requires the handler be async even though we don't await
#[allow(clippy::unused_async)]
async fn request(
    Query(BrowserAPIToken {
        email,
        secret,
        client_id,
    }): Query<BrowserAPIToken>,
    tx: Sender<Option<(String, FronteggAPIToken)>>,
) -> impl IntoResponse {
    if !secret.is_empty() {
        tx.send(Some((email, FronteggAPIToken { client_id, secret })))
            .unwrap();
    } else {
        tx.send(None).unwrap();
    }

    (StatusCode::OK, "You can now close the tab.")
}

/// Log the user using the browser, generates an API token and saves the new profile data.
pub(crate) async fn login_with_browser(
    endpoint: &Endpoint,
    profile_name: &str,
) -> Result<(String, FronteggAPIToken)> {
    // Open the browser to login user.
    let url = endpoint.web_login_url(profile_name).to_string();
    if let Err(err) = open::that(&url) {
        bail!("An error occurred when opening '{}': {}", url, err)
    }

    // Start the server to handle the request response
    let (tx, mut result) = channel(1);
    let mut close = tx.subscribe();
    let app = Router::new().route("/", get(|body| request(body, tx)));
    mz_ore::task::spawn(|| "server_task", async {
        let addr = SocketAddr::from(([127, 0, 0, 1], 8808));
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .with_graceful_shutdown(async move {
                close.recv().await.ok();
            })
            .await
    });

    result
        .recv()
        .await
        .context("failed to retrive new profile")?
        .context("failed to login via browser")
}

/// Generates an API token using an access token
pub(crate) async fn generate_api_token(
    endpoint: &Endpoint,
    client: &Client,
    access_token_response: FronteggAuth,
    description: &String,
) -> Result<FronteggAPIToken, reqwest::Error> {
    let mut body = BTreeMap::new();
    body.insert("description", description);

    client
        .post(endpoint.api_token_url())
        .authenticate(&access_token_response)
        .json(&body)
        .send()
        .await?
        .json::<FronteggAPIToken>()
        .await
}

/// Generates an access token using an API token
async fn authenticate_user(
    endpoint: &Endpoint,
    client: &Client,
    email: &str,
    password: &str,
) -> Result<FronteggAuth> {
    let mut access_token_request_body = BTreeMap::new();
    access_token_request_body.insert("email", email);
    access_token_request_body.insert("password", password);

    let response = client
        .post(endpoint.user_auth_url())
        .json(&access_token_request_body)
        .send()
        .await?;

    match response.status() {
        StatusCode::UNAUTHORIZED => {
            bail!("Invalid user or password")
        }
        _ => response
            .json::<FronteggAuth>()
            .await
            .context("failed to parse response from server"),
    }
}

/// Log the user using the console, generates an API token and saves the new profile data.

pub(crate) async fn login_with_console(
    endpoint: &Endpoint,
    client: &Client,
) -> Result<(String, FronteggAPIToken)> {
    // Handle interactive user input
    let mut email = String::new();

    print!("Email: ");
    let _ = std::io::stdout().flush();
    std::io::stdin().read_line(&mut email).unwrap();
    trim_newline(&mut email);

    print!("Password: ");
    let _ = std::io::stdout().flush();
    let password = rpassword::read_password().unwrap();

    let auth_user = authenticate_user(endpoint, client, &email, &password).await?;
    let api_token = generate_api_token(
        endpoint,
        client,
        auth_user,
        &String::from("App password for the CLI"),
    )
    .await?;

    Ok((email, api_token))
}
