//! The Security section of the server page: a username and password that
//! become the service's `server.auth` block with an Argon2id hash. The page
//! never carries the hash.

use maud::{html, Markup};
use rusty_photon_config::actions::FieldError;
use serde_json::Value;

use crate::form::FormValues;

/// The form control names of the section.
pub const USERNAME: &str = "auth.username";
pub const PASSWORD: &str = "auth.password";
pub const REPEAT: &str = "auth.password_repeat";

/// What the section shows: the stored username and whether a hash is stored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthView {
    pub username: String,
    pub has_hash: bool,
}

impl AuthView {
    /// Read the view from the config value's auth block at `pointer`.
    #[must_use]
    pub fn from_config(config: &Value, pointer: &str) -> Self {
        let block = config.pointer(pointer);
        Self {
            username: block
                .and_then(|b| b.get("username"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            has_hash: block
                .and_then(|b| b.get("password_hash"))
                .and_then(Value::as_str)
                .is_some_and(|h| !h.is_empty()),
        }
    }
}

/// The section's markup. `submitted` keeps a rejected submission's username
/// on the re-rendered form; the password inputs are always empty.
#[must_use]
pub fn render(view: &AuthView, submitted: Option<&FormValues>, errors: &[FieldError]) -> Markup {
    let username = submitted
        .and_then(|f| f.get(USERNAME))
        .unwrap_or(&view.username);
    let error_for = |name: &str| {
        errors
            .iter()
            .find(|e| e.path == name)
            .map(|e| e.msg.clone())
    };
    let username_error = error_for(USERNAME);
    let password_error = error_for(PASSWORD);
    let repeat_error = error_for(REPEAT);
    html! {
        fieldset {
            legend { "Security" }
            p.hint {
                @if view.has_hash {
                    "Requests need this username and password. Leave the password blank to keep the stored one; clear the username and the password fields to require no credential."
                } @else {
                    "No credential is required. Set a username and a password to require one."
                }
            }
            div.field.invalid[username_error.is_some()] {
                label for=(USERNAME) { "Username" }
                input type="text" id=(USERNAME) name=(USERNAME) value=(username) autocomplete="username";
                @if let Some(e) = &username_error { div.error { (e) } }
            }
            div.field.invalid[password_error.is_some()] {
                label for=(PASSWORD) { "Password" }
                input type="password" id=(PASSWORD) name=(PASSWORD) value="" autocomplete="new-password";
                @if let Some(e) = &password_error { div.error { (e) } }
                @else if view.has_hash { div.hint { "Blank keeps the stored password." } }
            }
            div.field.invalid[repeat_error.is_some()] {
                label for=(REPEAT) { "Repeat the password" }
                input type="password" id=(REPEAT) name=(REPEAT) value="" autocomplete="new-password";
                @if let Some(e) = &repeat_error { div.error { (e) } }
            }
        }
    }
}

/// Whether `change` alters the credential `stored` describes: a new
/// password always does, turning auth off does when it was on, and keeping
/// the stored password does when the username changes.
#[must_use]
pub fn changes(change: &AuthChange, stored: &AuthView) -> bool {
    match change {
        AuthChange::Untouched => false,
        AuthChange::Off => stored.has_hash || !stored.username.is_empty(),
        AuthChange::Keep { username } => *username != stored.username,
        AuthChange::Set { .. } => true,
    }
}

/// What a submission asks of the auth block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthChange {
    /// The section was not submitted (a page without it): leave the block.
    Untouched,
    /// A blank username: require no credential.
    Off,
    /// A username with a blank password: keep the stored hash.
    Keep { username: String },
    /// A username and a new password.
    Set { username: String, password: String },
}

/// Read the section's submission, by the rules the page states.
///
/// # Errors
///
/// A username with no password when none is stored, or a repeat that
/// differs from the password, each named on its field.
pub fn parse(form: &FormValues, stored_hash: bool) -> Result<AuthChange, Vec<FieldError>> {
    let Some(username) = form.get(USERNAME) else {
        return Ok(AuthChange::Untouched);
    };
    let username = username.trim();
    let password = form.get(PASSWORD).unwrap_or_default();
    let repeat = form.get(REPEAT).unwrap_or_default();
    if username.is_empty() {
        // A typed password with no username is a contradiction, not a
        // request to turn authentication off.
        if !password.is_empty() || !repeat.is_empty() {
            return Err(vec![FieldError {
                path: USERNAME.to_string(),
                msg: "a password needs a username; clear the password fields to require no credential"
                    .to_string(),
            }]);
        }
        return Ok(AuthChange::Off);
    }
    if password.is_empty() {
        if repeat.is_empty() && stored_hash {
            return Ok(AuthChange::Keep {
                username: username.to_string(),
            });
        }
        if repeat.is_empty() {
            return Err(vec![FieldError {
                path: PASSWORD.to_string(),
                msg: "a username needs a password; none is stored yet".to_string(),
            }]);
        }
    }
    if password != repeat {
        return Err(vec![FieldError {
            path: REPEAT.to_string(),
            msg: "does not match the password".to_string(),
        }]);
    }
    Ok(AuthChange::Set {
        username: username.to_string(),
        password: password.to_string(),
    })
}

/// Apply the change to the config value's auth block at `pointer`, hashing a
/// new password on the blocking pool.
///
/// # Errors
///
/// A hashing failure, named on the password field.
pub async fn apply(
    change: AuthChange,
    config: &mut Value,
    pointer: &str,
) -> Result<(), FieldError> {
    match change {
        AuthChange::Untouched => Ok(()),
        AuthChange::Off => {
            crate::form::set_pointer(config, pointer, Value::Null);
            Ok(())
        }
        AuthChange::Keep { username } => {
            crate::form::set_pointer(
                config,
                &format!("{pointer}/username"),
                Value::String(username),
            );
            Ok(())
        }
        AuthChange::Set { username, password } => {
            // Argon2id exists to be expensive; never on the runtime's workers.
            let hashed =
                tokio::task::spawn_blocking(move || rp_auth::credentials::hash_password(&password))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()))
                    .map_err(|e| FieldError {
                        path: PASSWORD.to_string(),
                        msg: format!("could not hash the password: {e}"),
                    })?;
            crate::form::set_pointer(
                config,
                pointer,
                serde_json::json!({ "username": username, "password_hash": hashed }),
            );
            Ok(())
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    fn form_from(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn the_view_reads_the_username_and_whether_a_hash_is_stored() {
        let config =
            json!({ "server": { "auth": { "username": "obs", "password_hash": "$argon2id$x" } } });
        assert_eq!(
            AuthView::from_config(&config, "/server/auth"),
            AuthView {
                username: "obs".to_string(),
                has_hash: true
            }
        );
        let off = json!({ "server": { "port": 1 } });
        assert_eq!(
            AuthView::from_config(&off, "/server/auth"),
            AuthView::default()
        );
    }

    #[test]
    fn the_section_never_carries_the_hash() {
        let view = AuthView {
            username: "obs".to_string(),
            has_hash: true,
        };
        let markup = render(&view, None, &[]).into_string();
        assert!(
            markup.contains(r#"name="auth.username" value="obs""#),
            "{markup}"
        );
        assert!(
            markup.contains(r#"name="auth.password" value="""#),
            "{markup}"
        );
        assert!(
            markup.contains(r#"name="auth.password_repeat" value="""#),
            "{markup}"
        );
        assert!(!markup.contains("argon2"), "{markup}");
        assert!(
            markup.contains("Blank keeps the stored password"),
            "{markup}"
        );
    }

    #[test]
    fn a_rejected_submission_keeps_its_username_and_shows_the_error() {
        let view = AuthView::default();
        let form = form_from(&[(USERNAME, "typed"), (PASSWORD, ""), (REPEAT, "")]);
        let errors = parse(&form, false).unwrap_err();
        let markup = render(&view, Some(&form), &errors).into_string();
        assert!(markup.contains(r#"value="typed""#), "{markup}");
        assert!(markup.contains("a username needs a password"), "{markup}");
    }

    #[test]
    fn parse_follows_the_stated_rules() {
        assert_eq!(parse(&form_from(&[]), true).unwrap(), AuthChange::Untouched);
        assert_eq!(
            parse(&form_from(&[(USERNAME, "  ")]), true).unwrap(),
            AuthChange::Off
        );
        assert_eq!(
            parse(
                &form_from(&[(USERNAME, ""), (PASSWORD, ""), (REPEAT, "")]),
                true
            )
            .unwrap(),
            AuthChange::Off
        );
        let err = parse(
            &form_from(&[(USERNAME, ""), (PASSWORD, "pw"), (REPEAT, "pw")]),
            true,
        )
        .unwrap_err();
        assert_eq!(err[0].path, USERNAME);
        assert!(err[0].msg.contains("needs a username"), "{}", err[0].msg);
        assert_eq!(
            parse(
                &form_from(&[(USERNAME, "obs"), (PASSWORD, ""), (REPEAT, "")]),
                true
            )
            .unwrap(),
            AuthChange::Keep {
                username: "obs".to_string()
            }
        );
        let err = parse(
            &form_from(&[(USERNAME, "obs"), (PASSWORD, ""), (REPEAT, "")]),
            false,
        )
        .unwrap_err();
        assert_eq!(err[0].path, PASSWORD);
        let err = parse(
            &form_from(&[(USERNAME, "obs"), (PASSWORD, "a"), (REPEAT, "b")]),
            true,
        )
        .unwrap_err();
        assert_eq!(err[0].path, REPEAT);
        assert_eq!(
            parse(
                &form_from(&[(USERNAME, "obs"), (PASSWORD, "pw"), (REPEAT, "pw")]),
                false
            )
            .unwrap(),
            AuthChange::Set {
                username: "obs".to_string(),
                password: "pw".to_string()
            }
        );
    }

    #[tokio::test]
    async fn apply_writes_a_verifiable_hash_keeps_or_removes_the_block() {
        let mut config = json!({ "server": { "port": 1 } });
        apply(
            AuthChange::Set {
                username: "obs".to_string(),
                password: "secret".to_string(),
            },
            &mut config,
            "/server/auth",
        )
        .await
        .unwrap();
        let hash = config["server"]["auth"]["password_hash"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(config["server"]["auth"]["username"], "obs");
        assert!(rp_auth::credentials::verify_password("secret", &hash));

        apply(
            AuthChange::Keep {
                username: "renamed".to_string(),
            },
            &mut config,
            "/server/auth",
        )
        .await
        .unwrap();
        assert_eq!(config["server"]["auth"]["username"], "renamed");
        assert_eq!(config["server"]["auth"]["password_hash"], hash);

        apply(AuthChange::Untouched, &mut config, "/server/auth")
            .await
            .unwrap();
        assert_eq!(config["server"]["auth"]["password_hash"], hash);

        apply(AuthChange::Off, &mut config, "/server/auth")
            .await
            .unwrap();
        assert!(config["server"]["auth"].is_null());
    }
}
