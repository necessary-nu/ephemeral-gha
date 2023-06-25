use std::{collections::HashMap, io::Write};

use bollard::{
    container::{CreateContainerOptions, ListContainersOptions, RemoveContainerOptions},
    image::{BuildImageOptions, BuilderVersion, ListImagesOptions, TagImageOptions},
    service::{BuildInfoAux, ContainerSummary},
    system::EventsOptions,
    Docker, API_DEFAULT_VERSION,
};
use clap::Parser;
use futures_util::StreamExt;
use rand::{distributions::Alphanumeric, Rng};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use url::Url;

const LABEL_ID: &str = "nu.necessary.ephemeral-gha";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Parser)]
struct Args {
    #[arg(env = "GITHUB_TOKEN", short = 'G', long = "github-token")]
    github_token: Option<String>,

    #[arg(env = "DOCKER_HOST", short = 'S', long = "socket")]
    docker_socket_url: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Parser)]
enum Command {
    Run(RunArgs),
    Status(StatusArgs),
    Cleanup(CleanupArgs),
}

#[derive(Debug, Parser)]
struct RunArgs {
    #[arg(short, long, default_value = "4")]
    runners: u32,

    #[arg(short, long)]
    url: Url,
}

#[derive(Debug, Parser)]
struct StatusArgs {}

#[derive(Debug, Parser)]
struct CleanupArgs {
    #[arg(short, long)]
    force: bool,
}

fn connect(url: Option<&str>) -> Result<Docker, bollard::errors::Error> {
    if let Some(url) = url {
        Docker::connect_with_local(url, 120, API_DEFAULT_VERSION)
    } else {
        Docker::connect_with_local_defaults()
    }
}

#[derive(Debug, Deserialize)]
struct VersionResponse {
    tag_name: String,
}

/// Queries GitHub for the current version of GHA.
async fn current_gha_version(token: &str) -> Result<String, reqwest::Error> {
    let client = make_client(token)?;

    let response: VersionResponse = client
        .get("https://api.github.com/repos/actions/runner/releases/latest")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(response.tag_name.trim_start_matches("v").to_string())
}

async fn build_gha_container(docker: &Docker, github_token: &str) -> Result<(), anyhow::Error> {
    let dockerfile = include_str!("../images/ubuntu-22.04/Dockerfile.amd64");

    let mut header = tar::Header::new_gnu();
    header.set_path("Dockerfile").unwrap();
    header.set_size(dockerfile.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut tar = tar::Builder::new(Vec::new());
    tar.append(&header, dockerfile.as_bytes()).unwrap();

    let uncompressed = tar.into_inner().unwrap();
    let mut c = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    c.write_all(&uncompressed).unwrap();
    let compressed = c.finish().unwrap();

    let gha_version = current_gha_version(github_token).await?;

    let ts = chrono::Utc::now().timestamp();
    let id = format!("ephemeral-gha:{gha_version}");
    let mut buildargs = HashMap::new();
    buildargs.insert("RUNNER_VERSION", &*gha_version);

    let build_image_options = BuildImageOptions {
        t: &*id,
        dockerfile: "Dockerfile",
        version: BuilderVersion::BuilderBuildKit,
        pull: true,
        session: Some(format!("ephemeral-gha-build-{ts}")),
        labels: [(LABEL_ID, env!("CARGO_PKG_VERSION"))].into(),
        platform: "linux/amd64",
        buildargs,
        ..Default::default()
    };

    let mut stream = docker.build_image(build_image_options, None, Some(compressed.into()));

    let mut image_id: Option<String> = None;
    while let Some(value) = stream.next().await {
        match value {
            Ok(bollard::models::BuildInfo {
                aux: Some(BuildInfoAux::BuildKit(inner)),
                ..
            }) => {
                for vertex in inner.vertexes {
                    println!("{}", vertex.name);
                }
                for log in inner.logs {
                    print!("{}", String::from_utf8_lossy(&log.msg));
                }
            }
            Ok(bollard::models::BuildInfo {
                stream: Some(inner),
                ..
            }) => println!("{}", inner),
            Ok(bollard::models::BuildInfo {
                aux: Some(BuildInfoAux::Default(image_id_)),
                ..
            }) => {
                if let Some(id) = image_id_.id {
                    println!("Image ID: {}", id);
                    image_id = Some(id);
                }
            }
            Ok(other) => println!("{:?}", other),
            Err(e) => eprintln!("Error: {:?}", e),
        }
    }

    let tag_options = Some(TagImageOptions {
        tag: "latest",
        repo: "ephemeral-gha",
    });

    docker.tag_image(&image_id.unwrap(), tag_options).await?;

    Ok(())

    // docker.build_image(options, credentials, tar)
}

async fn list(socket: &Docker) -> Result<Vec<ContainerSummary>, bollard::errors::Error> {
    let mut filters = HashMap::new();
    filters.insert("label".to_string(), vec![LABEL_ID.to_string()]);
    let result = socket
        .list_containers::<String>(Some(ListContainersOptions {
            filters,
            all: true,
            ..Default::default()
        }))
        .await?;
    Ok(result)
}

async fn cleanup(docker: &Docker, force: bool) -> Result<(), bollard::errors::Error> {
    let containers = list(docker).await?;
    for container in containers {
        if force
            || container.state.as_deref() == Some("created")
            || container.state.as_deref() == Some("exited")
        {
            let options = Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            });
            docker
                .remove_container(&container.id.unwrap(), options)
                .await?;
        }
    }

    Ok(())
}

fn make_client(token: &str) -> Result<reqwest::Client, reqwest::Error> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Accept",
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static("2022-11-28"),
    );

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .user_agent(USER_AGENT)
        .build()?;

    Ok(client)
}

pub async fn generate_gha_org_token(org: &str, token: &str) -> Result<String, reqwest::Error> {
    let client = make_client(token)?;

    let response: RegTokenResponse = client
        .post(format!(
            "https://api.github.com/orgs/{org}/actions/runners/registration-token"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(response.token)
}

pub async fn generate_gha_repo_token(
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<String, reqwest::Error> {
    let client = make_client(token)?;

    let response: RegTokenResponse = client
        .post(format!(
            "https://api.github.com/repos/{owner}/{repo}/actions/runners/registration-token"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(response.token)
}

#[derive(Debug, Deserialize)]
struct RegTokenResponse {
    token: String,
    expires_at: String,
}

pub async fn run() -> anyhow::Result<()> {
    let Args {
        github_token,
        docker_socket_url,
        command,
    } = Args::parse();

    let Some(command) = command else {
        eprintln!("No command specified; run --help for usage.");
        std::process::exit(1);
    };

    let socket = connect(docker_socket_url.as_deref())?;

    match command {
        Command::Status(_) => {
            let result = list(&socket).await?;
            for c in result {
                // c.state
                println!(
                    "{} [{}]: {:?}",
                    &c.names.unwrap()[0].trim_start_matches("/"),
                    c.state.unwrap_or_default(),
                    c.labels
                )
            }
        }
        Command::Cleanup(args) => {
            cleanup(&socket, args.force).await?;
        }
        Command::Run(args) => {
            let Some(github_token) = github_token else {
                eprintln!("Github token required");
                std::process::exit(1);
            };

            println!("Cleaning up old runners...");
            cleanup(&socket, false).await?;

            println!("Building most recent image...");
            build_gha_container(&socket, &github_token).await?;

            println!("Creating ephemeral runners...");
            create_ephemeral_runners(&socket, &github_token, &args).await?;

            let mut filters = HashMap::new();
            filters.insert("label".to_string(), vec![LABEL_ID.to_string()]);
            let options = Some(EventsOptions {
                filters,
                ..Default::default()
            });

            let mut stream = socket.events(options);

            println!("Observing ephemeral runner deaths...");
            while let Some(event) = stream.next().await {
                match event {
                    Ok(x) => {
                        if x.action.as_deref() == Some("die") {
                            println!(
                                "{} died; spawning new worker!",
                                x.actor.unwrap().attributes.unwrap().get("name").unwrap()
                            );
                            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                            cleanup(&socket, false).await?;
                            create_ephemeral_runners(&socket, &github_token, &args).await?;

                            let socket0 = socket.clone();
                            let github_token0 = github_token.to_string();
                            tokio::spawn(async move {
                                let socket = socket0;
                                let github_token = github_token0;
                                build_gha_container(&socket, &github_token).await
                            });
                        }
                    }
                    Err(e) => eprintln!("Error: {:?}", e),
                }
            }
        }
    }

    Ok(())
}

async fn create_ephemeral_runners(
    socket: &Docker,
    github_token: &str,
    args: &RunArgs,
) -> Result<(), anyhow::Error> {
    let Some(mut path_segments) = args.url.path_segments() else {
        eprintln!("Invalid URL");
        std::process::exit(1);
    };

    let org_or_user = path_segments.next().unwrap();
    let repo = path_segments.next();

    let gha_token = if let Some(repo) = repo {
        generate_gha_repo_token(org_or_user, repo, &github_token).await?
    } else {
        generate_gha_org_token(org_or_user, &github_token).await?
    };

    let mut filters = HashMap::new();
    filters.insert("label".to_string(), vec![LABEL_ID.to_string()]);
    println!(
        "{:?}",
        socket
            .list_images(Some(ListImagesOptions {
                filters,
                ..Default::default()
            }))
            .await?
    );

    let containers = list(&socket).await?;

    for _ in 0..(args.runners.saturating_sub(containers.len() as u32)) {
        let r: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect::<String>()
            .to_lowercase();
        let name = format!("ephemeral-gha-runner-{r}");
        let env = Some(vec![
            format!("TOKEN={}", &gha_token),
            format!("URL={}", args.url),
        ]);
        let container = socket
            .create_container(
                Some(CreateContainerOptions {
                    name,
                    platform: None,
                }),
                bollard::container::Config {
                    image: Some("ephemeral-gha:latest".to_string()),
                    env,
                    labels: Some(
                        [(LABEL_ID.to_string(), env!("CARGO_PKG_VERSION").to_string())]
                            .into_iter()
                            .collect(),
                    ),
                    ..Default::default()
                },
            )
            .await?;
        socket
            .start_container::<String>(&container.id, None)
            .await?;
        println!("{:?}", container);
    }

    Ok(())
}
