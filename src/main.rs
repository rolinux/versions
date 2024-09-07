extern crate chrono;
extern crate jsonpath_lib as jsonpath;
extern crate lettre;
extern crate reqwest;
extern crate rusqlite;
extern crate serde;
extern crate serde_json;
extern crate thiserror;
extern crate tokio;

use chrono::{NaiveDate, NaiveDateTime, Utc};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use reqwest::Client;
use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, Result};
use serde_json::Value;
use std::env;
use thiserror::Error;

#[derive(Debug)]
struct Target {
    id: Option<i32>,
    name: String,
    target_type: String,
    url: String,
    jsonpath_line: Option<String>,
    current_version: Option<String>,
    released: Option<MyNaiveDate>,
}

#[derive(Error, Debug)]
enum AppError {
    #[error("Failed to execute SQL query: {0}")]
    SqlError(#[from] rusqlite::Error),
    #[error("Failed to perform HTTP request: {0}")]
    ReqwestError(#[from] reqwest::Error),
    #[error("Failed to parse JSON: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Invalid line number in jsonpath_line")]
    InvalidLineNumber,
    #[error("Version not found using JSONPath")]
    VersionNotFound,
    #[error("Unexpected error: {0}")]
    UnexpectedError(String),
    #[error("JsonPathError: {0}")]
    JsonPathError(#[from] jsonpath::JsonPathError),
}

// Newtype pattern to wrap NaiveDate
#[derive(Debug)]
struct MyNaiveDate(NaiveDate);

impl ToSql for MyNaiveDate {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput> {
        Ok(ToSqlOutput::from(self.0.to_string()))
    }
}

impl FromSql for MyNaiveDate {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_str().and_then(|s| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(MyNaiveDate)
                .map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
        })
    }
}

impl Target {
    fn select_all(conn: &Connection) -> Result<Vec<Target>, AppError> {
        let mut stmt = conn.prepare(
            "SELECT id, name, type, url, jsonpath_line, current_version, released FROM targets",
        )?;
        let target_iter = stmt.query_map([], |row| {
            Ok(Target {
                id: row.get(0)?,
                name: row.get(1)?,
                target_type: row.get(2)?,
                url: row.get(3)?,
                jsonpath_line: row.get(4)?,
                current_version: row.get(5)?,
                released: row.get(6)?,
            })
        })?;

        let mut targets = Vec::new();
        for target in target_iter {
            targets.push(target?);
        }

        Ok(targets)
    }

    async fn fetch_version(&self, client: &Client) -> Result<Option<String>, AppError> {
        let response = client
            .get(&self.url)
            .header("Accept", "application/json")
            .send()
            .await?
            .text()
            .await?;

        match self.target_type.as_str() {
            "json" => {
                if let Some(jsonpath) = &self.jsonpath_line {
                    let json: Value =
                        serde_json::from_str(&response).map_err(AppError::JsonError)?;
                    let mut selector = jsonpath::selector(&json);
                    if let Some(version) = selector(jsonpath)?.first() {
                        return Ok(Some(version.as_str().unwrap().to_string()));
                    }
                    return Err(AppError::VersionNotFound);
                }
            }
            "text" => {
                if let Some(line_number) = &self.jsonpath_line {
                    if let Ok(line_index) = line_number.parse::<usize>() {
                        if let Some(line) = response.lines().nth(line_index) {
                            return Ok(Some(line.to_string()));
                        }
                        return Err(AppError::VersionNotFound);
                    }
                    return Err(AppError::InvalidLineNumber);
                }
            }
            _ => (),
        }

        Ok(None)
    }

    async fn update(&self, conn: &Connection, new_version: &str) -> Result<(), AppError> {
        // Ensure the id is provided to target the specific row
        if let Some(id) = self.id {
            let today = MyNaiveDate(Utc::now().date_naive());

            // Copy current data to the versions table
            conn.execute(
                "INSERT INTO versions (target_id, version, released, updated, updated_version) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, self.current_version, self.released.as_ref(), &today, new_version],
            )?;

            // Update the targets table with the new version and today's date
            conn.execute(
                "UPDATE targets SET current_version = ?1, released = ?2 WHERE id = ?3",
                params![new_version, &today, id],
            )?;
            Ok(())
        } else {
            Err(AppError::UnexpectedError(
                "Target ID is missing".to_string(),
            ))
        }
    }
}

async fn send_email(subject: &str, body: &str) -> Result<(), Box<dyn std::error::Error>> {
    let smtp_username = env::var("SMTP_USERNAME").expect("SMTP_USERNAME not set");
    let smtp_password = env::var("SMTP_PASSWORD").expect("SMTP_PASSWORD not set");
    let recipient_email = env::var("RECIPIENT_EMAIL").expect("RECIPIENT_EMAIL not set");

    // Create an email message
    let email = Message::builder()
        .from(smtp_username.parse().unwrap())
        .to(recipient_email.parse().unwrap())
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .unwrap();

    // Set up SMTP credentials
    let creds = Credentials::new(smtp_username.clone(), smtp_password);

    // Create the SMTP transport
    let mailer = SmtpTransport::relay("smtp.gmail.com") // Replace with your SMTP server address
        .unwrap()
        .credentials(creds)
        .build();

    // Send the email
    mailer.send(&email)?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    let db_path = env::var("SQLITE_DB_PATH").expect("SQLITE_DB_PATH not set");
    let conn = Connection::open(db_path)?;
    let client = Client::new();

    // Select all targets
    let targets = Target::select_all(&conn)?;
    for target in &targets {
        // Fetch the new version from the target's URL
        if let Some(new_version) = target.fetch_version(&client).await? {
            if let Some(current_version) = &target.current_version {
                if new_version != *current_version {
                    // Calculate the number of days since the last release
                    let days_since_release = if let Some(released_date) = &target.released {
                        let released_datetime = NaiveDateTime::new(
                            released_date.0,
                            chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                        );
                        let duration = Utc::now()
                            .naive_utc()
                            .signed_duration_since(released_datetime);
                        duration.num_days()
                    } else {
                        0
                    };

                    // Update the target if the version is newer
                    target.update(&conn, &new_version).await?;

                    // Prepare the email content
                    let subject = format!("New version for target: {}", target.name);
                    let body = format!(
                        "Target: {}\nOld Version: {}\nNew Version: {}\nDays Since Last Release: {}",
                        target.name, current_version, new_version, days_since_release
                    );

                    // Send the email
                    send_email(&subject, &body)
                        .await
                        .map_err(|e| AppError::UnexpectedError(e.to_string()))?;

                    println!(
                        "Updated target: {} to version: {}",
                        target.name, new_version
                    );
                } else {
                    println!(
                        "Target: {} version is unchanged: {}",
                        target.name, current_version
                    );
                }
            } else {
                // Handle case where there is no current version
                target.update(&conn, &new_version).await?;

                // Prepare the email content
                let subject = format!("New version for target: {}", target.name);
                let body = format!("Target: {}\nNew Version: {}", target.name, new_version);

                // Send the email
                send_email(&subject, &body)
                    .await
                    .map_err(|e| AppError::UnexpectedError(e.to_string()))?;

                println!(
                    "Updated target: {} to version: {}",
                    target.name, new_version
                );
            }
        } else {
            println!("No new version found for target: {}", target.name);
        }
    }

    Ok(())
}
