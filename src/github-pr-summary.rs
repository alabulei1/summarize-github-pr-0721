use dotenv::dotenv;
use flowsnet_platform_sdk::logger;
use github_flows::{
    get_octo, listen_to_event,
    octocrab::models::events::payload::IssueCommentEventAction,
    octocrab::models::CommentId,
    EventPayload, GithubLogin
};
use openai_flows::{
    chat::{ChatModel, ChatOptions},
    OpenAIFlows, FlowsAccount,
};
use std::env;

//  The soft character limit of the input context size
//   the max token size or word count for GPT4 is 8192
//   the max token size or word count for GPT35Turbo is 4096
static CHAR_SOFT_LIMIT : usize = 18000;
// static MODEL : ChatModel = ChatModel::GPT35Turbo;
static MODEL : ChatModel = ChatModel::GPT4;

#[no_mangle]
#[tokio::main(flavor = "current_thread")]
pub async fn run() -> anyhow::Result<()> {
    dotenv().ok();
    logger::init();
    log::debug!("Running github-pr-summary/playground");

    let owner = env::var("github_owner").unwrap_or("juntao".to_string());
    let repo = env::var("github_repo").unwrap_or("test".to_string());
    let trigger_phrase = env::var("trigger_phrase").unwrap_or("flows summarize".to_string());

    let events = vec!["pull_request", "issue_comment"];
    listen_to_event(&GithubLogin::Default, &owner, &repo, events, |payload| {
        handler(
            &owner,
            &repo,
            &trigger_phrase,
            payload,
        )
    })
    .await;

    Ok(())
}

async fn handler(
    owner: &str,
    repo: &str,
    trigger_phrase: &str,
    payload: EventPayload,
) {
    let (pull_url, issue_number, _contributor) = match payload {
        EventPayload::IssueCommentEvent(e) => {
            if e.action == IssueCommentEventAction::Deleted {
                log::debug!("Deleted issue comment");
                return;
            }
            log::debug!("Other event for issue comment");

            let body = e.comment.body.unwrap_or_default();

            // if e.comment.performed_via_github_app.is_some() {
            //     return;
            // }
            // TODO: Makeshift but operational
            if body.starts_with("Hello, I am a [code review bot]") {
                log::info!("Ignore comment via bot");
                return;
            };

            if !body.to_lowercase().contains(&trigger_phrase.to_lowercase()) {
                log::info!("Ignore the comment without the magic words");
                return;
            }

            (e.issue.title, e.issue.number, e.issue.user.login)
        }
        _ => return,
    };

    let pull_url_components : Vec<&str> = pull_url.split("/").collect();
    if pull_url_components.len() < 5 { return; }
    let pull_number = pull_url_components[pull_url_components.len() - 1].parse::<u64>().unwrap();
    let pull_repo = pull_url_components[pull_url_components.len() - 3];
    let pull_owner = pull_url_components[pull_url_components.len() - 4];

    let octo = get_octo(&GithubLogin::Default);
    let issues = octo.issues(owner, repo);
    let comment_id: CommentId;
    match issues.create_comment(issue_number, "Hello, I am a [code review bot](https://github.com/flows-network/github-pr-summary/) on [flows.network](https://flows.network/).\n\nIt could take a few minutes for me to analyze this PR. Relax, grab a cup of coffee and check back later. Thanks!").await {
        Ok(comment) => {
            comment_id = comment.id;
        }
        Err(error) => {
            log::error!("Error posting comment: {}", error);
            return;
        }
    }

    let pulls = octo.pulls(pull_owner, pull_repo);
    let patch_as_text = pulls.get_patch(pull_number).await.unwrap();
    let mut current_commit = String::new();
    let mut commits: Vec<String> = Vec::new();
    for line in patch_as_text.lines() {
        if line.starts_with("From ") {
            // Detected a new commit
            if !current_commit.is_empty() {
                // Store the previous commit
                commits.push(current_commit.clone());
            }
            // Start a new commit
            current_commit.clear();
        }
        // Append the line to the current commit if the current commit is less than CHAR_SOFT_LIMIT
        if current_commit.len() < CHAR_SOFT_LIMIT {
            current_commit.push_str(line);
            current_commit.push('\n');
        }
    }
    if !current_commit.is_empty() {
        // Store the last commit
        commits.push(current_commit.clone());
    }

    if commits.is_empty() {
        log::error!("Cannot parse any commit from the patch file");
        return;
    }

    let chat_id = format!("PR#{pull_number}");
    let system = "You are an experienced software developer. You will act as a reviewer for a GitHub Pull Request.";
    let mut openai = OpenAIFlows::new();
    openai.set_flows_account(FlowsAccount::Provided("gpt4".to_string()));
    openai.set_retry_times(3);

    let mut reviews: Vec<String> = Vec::new();
    let mut reviews_text = String::new();
    for (_i, commit) in commits.iter().enumerate() {
        let commit_hash = &commit[5..45];
        log::debug!("Sending patch to OpenAI: {}", commit_hash);
        let co = ChatOptions {
            model: MODEL,
            restart: true,
            system_prompt: Some(system),
        };
        let question = "The following is a GitHub patch. Please summarize the key changes and identify potential problems. Start with the most important findings.\n\n".to_string() + truncate(commit, CHAR_SOFT_LIMIT);
        match openai.chat_completion(&chat_id, &question, &co).await {
            Ok(r) => {
                if reviews_text.len() < CHAR_SOFT_LIMIT {
                    reviews_text.push_str("------\n");
                    reviews_text.push_str(&r.choice);
                    reviews_text.push_str("\n");
                }
                let mut review = String::new();
                review.push_str(&format!("### [Commit {commit_hash}](https://github.com/WasmEdge/WasmEdge/pull/{pull_number}/commits/{commit_hash})\n"));
                review.push_str(&r.choice);
                review.push_str("\n\n");
                reviews.push(review);
                log::debug!("Received OpenAI resp for patch: {}", commit_hash);
            }
            Err(e) => {
                log::error!("OpenAI returned an error for commit {commit_hash}: {}", e);
            }
        }
    }

    let mut resp = String::new();
    resp.push_str("Hello, I am a [code review bot](https://github.com/flows-network/github-pr-summary/) on [flows.network](https://flows.network/). Here are my reviews of code commits in this PR.\n\n------\n\n");
    if reviews.len() > 1 {
        log::debug!("Sending all reviews to OpenAI for summarization");
        let co = ChatOptions {
            model: MODEL,
            restart: true,
            system_prompt: Some(system),
        };
        let question = "Here is a set of summaries for software source code patches. Each summary starts with a ------ line. Please write an overall summary considering all the individual summary. Please present the potential issues and errors first, following by the most important findings, in your summary.\n\n".to_string() + &reviews_text;
        match openai.chat_completion(&chat_id, &question, &co).await {
            Ok(r) => {
                resp.push_str(&r.choice);
                resp.push_str("\n\n## Details\n\n");
                log::debug!("Received the overall summary");
            }
            Err(e) => {
                log::error!("OpenAI returned an error for the overall summary: {}", e);
            }
        }
    }
    for (_i, review) in reviews.iter().enumerate() {
        resp.push_str(review);
    }

    resp.push_str("cc ");
    resp.push_str(&pull_url);
    resp.push_str("\n");

    // Send the entire response to GitHub PR
    match issues.update_comment(comment_id, resp).await {
        Err(error) => {
            log::error!("Error posting resp: {}", error);
        }
        _ => {}
    }
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}
