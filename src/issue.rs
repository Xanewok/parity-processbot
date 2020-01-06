use crate::db::*;
use crate::{
	constants::*,
	error,
	github,
	github_bot::GithubBot,
	matrix,
	matrix_bot::MatrixBot,
	project,
	Result,
};
use itertools::Itertools;
use rocksdb::DB;
use snafu::{
	GenerateBacktrace,
	OptionExt,
	ResultExt,
};
use std::collections::HashMap;
use std::time::{
	Duration,
	SystemTime,
};

fn issue_actor_and_project(
	issue: &github::Issue,
	github_bot: &GithubBot,
) -> Result<Option<(github::User, github::Project)>> {
	let repo = &issue.repository;
	let issue_number = issue.number.context(error::MissingData)?;
	github_bot
		.issue_events(&repo.name, issue_number)
		.map(|issue_events| {
			issue_events
				.iter()
				.sorted_by_key(|ie| ie.created_at)
				.rev()
				.find(|issue_event| {
					issue_event.event == Some(github::Event::AddedToProject)
						|| issue_event.event
							== Some(github::Event::RemovedFromProject)
				})
				.and_then(|issue_event| {
					if issue_event.event == Some(github::Event::AddedToProject)
					{
						issue_event.project_card.as_ref().and_then(|card| {
							card.project_url.as_ref().map(|project_url| {
								(issue_event.actor.clone(), project_url)
							})
						})
					} else {
						None
					}
				})
				.and_then(|(actor, project_url)| {
					github_bot
						.get(project_url)
						.ok()
						.map(|project| (actor, project))
				})
		})
}

pub fn handle_issue(
	db: &DB,
	github_bot: &GithubBot,
	matrix_bot: &MatrixBot,
	core_devs: &[github::User],
	github_to_matrix: &HashMap<String, String>,
	projects: Option<&project::Projects>,
	issue: &github::Issue,
	default_channel_id: &str,
) -> Result<()> {
	// TODO: handle multiple projects in a single repo

	let issue_id = issue.id.context(error::MissingData)?;
	let issue_html_url = issue.html_url.as_ref().context(error::MissingData)?;

	let db_key = &format!("{}", issue_id).into_bytes();
	let mut db_entry = DbEntry::new();
	if let Ok(Some(entry)) = db.get_pinned(db_key).map(|v| {
		v.map(|value| {
			serde_json::from_str::<DbEntry>(
				String::from_utf8(value.to_vec()).unwrap().as_str(),
			)
			.expect("deserialize entry")
		})
	}) {
		db_entry = entry;
	}

	let author = &issue.user;
	let author_is_core = core_devs.iter().find(|u| u.id == author.id).is_some();

	let repo = &issue.repository;

	match if projects.map_or(true, |p| p.0.is_empty()) {
		unimplemented!()
	} else {
		match issue_actor_and_project(issue, github_bot)? {
			None => {
				let since = db_entry
					.issue_no_project_ping
					.and_then(|ping| ping.elapsed().ok());

				if author_is_core {
					let ticks = since.map(|elapsed| {
						elapsed.as_secs() / ISSUE_NO_PROJECT_CORE_PING_PERIOD
					});
					match ticks {
						None => {
							db_entry.issue_no_project_ping =
								Some(SystemTime::now());
							matrix_bot.send_public_message(
								default_channel_id,
								&ISSUE_NO_PROJECT_MESSAGE
									.replace("{1}", issue_html_url),
							);
							DbEntryState::Update
						}
						Some(0) => DbEntryState::DoNothing,
						Some(i) => {
							if i == ISSUE_NO_PROJECT_ACTION_AFTER_NPINGS {
								// If after 3 days there is still no project
								// attached, move the issue to Core Sorting
								// repository
								github_bot.close_issue(&repo.name, issue_id);
								github_bot.create_issue(
									CORE_SORTING_REPO,
									serde_json::json!({ "title": issue.title, "body": issue.body.as_ref().unwrap_or(&"".to_owned()) }),
								);
								DbEntryState::Delete
							} else if (db_entry.issue_no_project_npings) < i {
								db_entry.issue_no_project_npings = i;
								matrix_bot.send_public_message(
									default_channel_id,
									&ISSUE_NO_PROJECT_MESSAGE
										.replace("{1}", issue_html_url),
								);
								DbEntryState::Update
							} else {
								DbEntryState::DoNothing
							}
						}
					}
				} else {
					// ..otherwise, sent a message to the "Core Developers" room
					// on Riot with the title of the issue and a link.
					// If after 15 minutes there is still no project attached,
					// move the issue to Core Sorting repository.
					let ticks = since.map(|elapsed| {
						elapsed.as_secs()
							/ ISSUE_NO_PROJECT_NON_CORE_PING_PERIOD
					});
					match ticks {
						None => {
							db_entry.issue_no_project_ping =
								Some(SystemTime::now());
							matrix_bot.send_public_message(
								default_channel_id,
								&ISSUE_NO_PROJECT_MESSAGE
									.replace("{1}", issue_html_url),
							);
							DbEntryState::Update
						}
						Some(0) => DbEntryState::DoNothing,
						_ => {
							// If after 15 minutes there is still no project
							// attached, move the issue to Core Sorting
							// repository.
							github_bot.close_issue(&repo.name, issue_id);
							github_bot.create_issue(
                                                                CORE_SORTING_REPO, 
                                                                serde_json::json!({ "title": issue.title, "body": issue.body.as_ref().unwrap_or(&"".to_owned()) })
                                                        );
							DbEntryState::Delete
						}
					}
				}
			}
			Some((actor, project)) => unimplemented!(),
		}
	} {
		DbEntryState::Delete => {
			db.delete(db_key).context(error::Db)?;
		}
		DbEntryState::Update => {
			db.delete(db_key).context(error::Db)?;
			db.put(
				db_key,
				serde_json::to_string(&db_entry)
					.expect("serialize db entry")
					.as_bytes(),
			)
			.unwrap();
		}
		_ => {}
	}

	Ok(())
}
