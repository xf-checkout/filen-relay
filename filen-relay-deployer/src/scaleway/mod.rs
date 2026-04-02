use anyhow::{Context, Result};
use filen_cli::serialize_auth_config;
use filen_sdk_rs::auth::Client;

use crate::Args;

mod scaleway_api;

pub(crate) async fn deploy_to_scaleway(
	filen_relay_version: &str,
	client: Client,
	args: Args,
) -> Result<()> {
	// print help
	cliclack::note(
		"Deploy to Scaleway",
		"You can deploy Filen Relay to Scaleway as a Scaleway Serverless Container
(read more about it here: https://www.scaleway.com/en/serverless-containers/)

If you don't have one already, createa an account and register a payment
method. Filen Relay is designed to use minimal resouces, so it should be
cheap to run (check pricing: https://www.scaleway.com/en/pricing/serverless/).

For the next step, you will need an API key, which you can generate at
https://console.scaleway.com/iam/api-keys.
The Organization ID can be found on the same page.",
	)?;

	// enter api key, organization id, region
	let api_key: String = match args.scaleway_api_key_secret {
		Some(ref api_key) => api_key.clone(),
		None => cliclack::password("Enter your Scaleway API Secret Key:").interact()?,
	};
	let organization_id: String = match args.scaleway_organization_id {
		Some(ref organization_id) => organization_id.clone(),
		None => cliclack::input("Enter your Scaleway Organization ID:").interact()?,
	};
	let region = match args.scaleway_region {
		Some(ref region) => region,
		None => cliclack::select("Enter the region to deploy to")
			.item("fr-par", "Paris (fr-par)", "")
			.item("nl-ams", "Amsterdam (nl-ams)", "")
			.item("pl-waw", "Warsaw (pl-waw)", "")
			.interact()?,
	};
	let scaleway = scaleway_api::ScalewayApi::new(&api_key, &organization_id, region);

	// choose project
	let projects = scaleway
		.list_projects()
		.await
		.context("Failed to list Scaleway projects")?;
	let project_id = match args.scaleway_project_id {
		Some(ref project_id) => project_id,
		None => cliclack::select("Choose a project to deploy to:")
			.items(
				projects
					.iter()
					.map(|p| (p.id.as_str(), p.name.as_str(), ""))
					.collect::<Vec<_>>()
					.as_slice(),
			)
			.interact()?,
	};

	// choose "filen-relay" namespace or create it
	let namespaces = scaleway
		.list_containers_namespaces()
		.await
		.context("Failed to list Scaleway namespaces")?;
	let namespace_id = match args.scaleway_namespace_id {
		Some(ref namespace_id) => namespace_id,
		None => cliclack::select("Choose a namespace to deploy to:")
			.item("create_new", "Create a new namespace", "")
			.items(
				namespaces
					.iter()
					.map(|ns| (ns.id.as_str(), ns.name.as_str(), ""))
					.collect::<Vec<_>>()
					.as_slice(),
			)
			.interact()?,
	};
	let namespace = if namespace_id == "create_new" {
		// create a new namespace named "filen-relay-<random-suffix>"
		let random_suffix: String = uuid::Uuid::new_v4().as_simple().to_string()[..8].to_string();
		let namespace_name = format!("filen-relay-{}", random_suffix);
		scaleway
			.create_containers_namespace(&namespace_name, project_id)
			.await
			.context("Failed to create Scaleway namespace")?
	} else {
		let namespace_id = namespace_id.to_string();
		namespaces
			.into_iter()
			.find(|ns| ns.id == namespace_id)
			.unwrap()
	};

	// wait for namespace to be ready
	let namespace_ready_spinner = cliclack::spinner();
	let mut i = 0;
	loop {
		let namespace = scaleway
			.get_containers_namespace(&namespace.id)
			.await
			.context("Failed to get Scaleway namespace while waiting for it to be ready")?;
		if namespace.status == "ready" {
			break;
		}
		if i == 1 {
			namespace_ready_spinner.start("Waiting for namespace to be ready...");
		}
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		i += 1;
	}
	namespace_ready_spinner.stop("Namespace is ready!");

	// get existing containers starting with "filen-relay-" and ask user if they want to update them
	let containers = scaleway
		.list_containers()
		.await
		.context("Failed to list Scaleway containers")?;
	let filen_relay_containers: Vec<_> = containers
		.into_iter()
		.filter(|c| c.name.starts_with("filen-relay-"))
		.collect();
	let container_to_update = if filen_relay_containers.is_empty() {
		None
	} else {
		cliclack::select("Do you want to update an existing container?")
			.item(None, "Create a new container", "")
			.items(
				&filen_relay_containers
					.into_iter()
					.map(|c| {
						let label = format!("Update container: {}", c.name);
						(Some(c), label, "")
					})
					.collect::<Vec<_>>(),
			)
			.interact()?
	};

	let container_config = serde_json::json!({
		"namespace_id": namespace.id,
		"name": match container_to_update {
			Some(ref c) => c.name.clone(),
			None => format!(
				"filen-relay-{}",
				&uuid::Uuid::new_v4().as_simple().to_string()[..8]
			),
		},
		"registry_image": format!("ghcr.io/filenclouddienste/filen-relay:{}", filen_relay_version),
		"min_scale": 0,
		"max_scale": 1,
		"port": 80,
		"cpu_limit": 250,
		"memory_limit": 256,
		"secret_environment_variables": [
			{
				"key": "FILEN_RELAY_ADMIN_AUTH_CONFIG",
				"value": serialize_auth_config(&client)?,
			},
		],
		"health_check": {
			"http": {
				"path": "/api/ready",
			},
			"failure_threshold": 24,
			"interval": "5s"
		},
	});

	let container = if let Some(container) = container_to_update {
		// update existing container
		scaleway
			.update_container(&container.id, &container_config)
			.await
			.context("Failed to update Scaleway container")?;
		container
	} else {
		// create new container and deploy it
		let container = scaleway
			.create_container(&container_config)
			.await
			.context("Failed to create Scaleway container")?;
		scaleway
			.deploy_container(&container.id)
			.await
			.context("Failed to deploy Scaleway container")?;
		container
	};

	// display success message with links
	let console_url = format!(
		"https://console.scaleway.com/containers/namespaces/{}/{}/containers/{}",
		region, namespace.id, container.id
	);
	cliclack::log::success(format!(
        "Deployed Filen Relay to Scaleway!\nView it in the Scaleway Console: {}\nFilen Relay soon available at: https://{}",
        console_url,
        container.domain_name
    ))?;

	Ok(())
}
