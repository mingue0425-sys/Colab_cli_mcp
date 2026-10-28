//! Account profiles: several Google accounts side by side, each with its own
//! sign-in, slots, settings and idle watcher. Each account works within its
//! own Colab quota; colab-cli never moves work between accounts on its own.

use crate::{
    auth::Auth,
    config,
    error::{Error, Result},
    google,
};
use serde_json::{Value, json};

fn auth(name: &str) -> Auth {
    Auth::new(config::account_home(name), google::client())
}

pub fn list(current: &str) -> Value {
    let accounts: Vec<Value> = config::accounts()
        .iter()
        .map(|name| {
            let email = auth(name).email();
            json!({"account": name, "email": email, "signed_in": email.is_some(), "in_use": name == current})
        })
        .collect();
    json!({
        "accounts": accounts,
        "next_step": "Add one with `colab --account NAME auth login`; switch with `colab auth use NAME` or pass --account.",
    })
}

/// Make `name` the default account. It must be signed in, so a typo cannot
/// silently send later commands to an empty profile.
pub fn use_account(name: &str) -> Result<Value> {
    config::check_account(name)?;
    let Some(email) = auth(name).email() else {
        return Err(Error::new(
            "auth_required",
            format!("Account {name} is not signed in."),
            format!("Run `colab --account {name} auth login` first."),
        ));
    };
    let root = config::root();
    config::private_dir(&root)?;
    config::write_private(&root.join("account"), name.as_bytes())?;
    Ok(json!({"account": name, "email": email, "in_use": true}))
}

/// The Drive API of another signed-in account, for saving results there.
pub fn api(name: &str) -> Result<google::Api> {
    config::check_account(name)?;
    let home = config::account_home(name);
    if auth(name).email().is_none() {
        return Err(Error::new(
            "auth_required",
            format!("Account {name} is not signed in."),
            format!("Run `colab --account {name} auth login` first, or check `colab auth list`."),
        ));
    }
    Ok(google::Api::new(home))
}
