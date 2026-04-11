use anyhow::{Context, Result, bail};
use argon2::{
    Argon2,
    password_hash::{PasswordHasher, SaltString},
};
use ring::rand::{SecureRandom, SystemRandom};
use sqlx::SqlitePool;
use std::io::{self, BufRead};

pub enum AdminCommand {
    Help,
    ResetPassword { username: String },
}

pub fn parse(args: &[String]) -> Result<Option<AdminCommand>> {
    if args.is_empty() {
        return Ok(None);
    }

    Ok(Some(parse_args(args)?))
}

pub async fn run(cmd: AdminCommand, db: &SqlitePool) -> Result<()> {
    match cmd {
        AdminCommand::Help => Ok(()),
        AdminCommand::ResetPassword { username } => reset_password(db, &username).await,
    }
}

pub fn print_usage() {
    println!("{}", usage());
}

fn parse_args(args: &[String]) -> Result<AdminCommand> {
    if args.is_empty() {
        bail!(usage());
    }

    if args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        return Ok(AdminCommand::Help);
    }

    if args[0] != "reset-password" {
        bail!(usage());
    }

    let mut username: Option<String> = None;
    let mut password_stdin = false;

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "--username" | "-u" => {
                i += 1;
                if i >= args.len() {
                    bail!("Missing value for --username\n\n{}", usage());
                }
                username = Some(args[i].trim().to_string());
            }
            "--password-stdin" => {
                password_stdin = true;
            }
            "--help" | "-h" => return Ok(AdminCommand::Help),
            other => bail!("Unknown option: {other}\n\n{}", usage()),
        }
        i += 1;
    }

    let username = username
        .filter(|u| !u.is_empty())
        .context("Missing required --username")?;

    if !password_stdin {
        bail!("Missing required --password-stdin\n\n{}", usage());
    }

    Ok(AdminCommand::ResetPassword { username })
}

fn usage() -> &'static str {
    "Usage:
  selu-orchestrator reset-password --username <username> --password-stdin

Example:
  echo 'new-very-long-password' | selu-orchestrator reset-password --username alice --password-stdin"
}

async fn reset_password(db: &SqlitePool, username: &str) -> Result<()> {
    let password = read_password_from_stdin()?;
    if password.len() < 8 {
        bail!("Password must be at least 8 characters long.");
    }

    let hash = hash_password(&password)?;

    let mut tx = db.begin().await.context("Failed to start DB transaction")?;

    let row = sqlx::query!("SELECT id FROM users WHERE username = ?", username)
        .fetch_optional(&mut *tx)
        .await
        .context("Failed to load user")?;

    let Some(row) = row else {
        bail!("User '{username}' not found.");
    };
    let user_id = row.id.context("User record missing ID")?;

    sqlx::query!(
        "UPDATE users SET password_hash = ? WHERE id = ?",
        hash,
        user_id
    )
    .execute(&mut *tx)
    .await
    .context("Failed to update password")?;

    let deleted = sqlx::query!("DELETE FROM web_sessions WHERE user_id = ?", user_id)
        .execute(&mut *tx)
        .await
        .context("Failed to revoke web sessions")?
        .rows_affected();

    tx.commit()
        .await
        .context("Failed to commit password reset")?;

    println!("Password reset for user '{username}'. Revoked {deleted} active web session(s).");
    Ok(())
}

fn read_password_from_stdin() -> Result<String> {
    let mut line = String::new();
    let mut stdin = io::stdin().lock();
    let bytes = stdin
        .read_line(&mut line)
        .context("Failed to read password from stdin")?;
    if bytes == 0 {
        bail!("No password received on stdin.");
    }
    Ok(line.trim_end_matches(&['\r', '\n'][..]).to_string())
}

fn hash_password(password: &str) -> Result<String> {
    let sys_rng = SystemRandom::new();
    let mut salt_bytes = [0u8; 16];
    if sys_rng.fill(&mut salt_bytes).is_err() {
        bail!("Failed to generate random salt");
    }
    let salt = match SaltString::encode_b64(&salt_bytes) {
        Ok(s) => s,
        Err(_) => bail!("Failed to encode salt"),
    };
    match Argon2::default().hash_password(password.as_bytes(), &salt) {
        Ok(h) => Ok(h.to_string()),
        Err(_) => bail!("Argon2 password hashing failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[test]
    fn parse_reset_password_args() {
        let args = vec![
            "reset-password".to_string(),
            "--username".to_string(),
            "alice".to_string(),
            "--password-stdin".to_string(),
        ];
        let res = parse_args(&args);
        assert!(res.is_ok());
    }

    #[test]
    fn parse_requires_password_stdin() {
        let args = vec![
            "reset-password".to_string(),
            "--username".to_string(),
            "alice".to_string(),
        ];
        let res = parse_args(&args);
        assert!(res.is_err());
    }
}
