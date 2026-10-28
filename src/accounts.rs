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

/// A sign-in result, noting when it replaced another Google account in this
/// profile or duplicates a Google account signed in under another profile.
pub fn signed_in(account: &str, before: Option<String>, mut result: Value) -> Value {
    result["account"] = account.into();
    let email = result["email"].as_str().unwrap_or_default().to_owned();
    let mut notes = Vec::new();
    if let Some(before) = before.filter(|b| !b.is_empty() && *b != email) {
        notes.push(format!(
            "This replaced {before} in profile {account}. To keep both, sign in under another name: `colab --account NAME auth login`."
        ));
        result["replaced"] = before.into();
    }
    let same: Vec<String> = config::accounts()
        .into_iter()
        .filter(|other| other != account && auth(other).email().as_deref() == Some(email.as_str()))
        .collect();
    if !same.is_empty() {
        notes.push(format!(
            "{email} is also signed in as profile {}; both profiles use the same Google account.",
            same.join(", ")
        ));
        result["also_profiles"] = same.into();
    }
    if !notes.is_empty() {
        result["note"] = notes.join(" ").into();
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_google_account_is_reported() {
        let first = signed_in("t-new", None, json!({"email": "a@x.com"}));
        assert!(first.get("replaced").is_none() && first.get("note").is_none());
        let again = signed_in("t-new", Some("a@x.com".into()), json!({"email": "a@x.com"}));
        assert!(again.get("replaced").is_none());
        let other = signed_in("t-new", Some("a@x.com".into()), json!({"email": "b@x.com"}));
        assert_eq!(other["replaced"], "a@x.com");
        assert!(other["note"].as_str().unwrap().contains("--account NAME"));
    }
}
