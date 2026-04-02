use anyhow::{Context, Result};
use reqwest::Method;
use serde::{Deserialize, Serialize};

pub(crate) struct ScalewayApi {
	client: reqwest::Client,
	organization_id: String,
	region: String,
}

#[derive(Deserialize)]
pub struct ListProjectsResponse {
	pub projects: Vec<ListProjectsResponseItem>,
}

#[derive(Deserialize)]
pub struct ListProjectsResponseItem {
	pub id: String,
	pub name: String,
}

#[derive(Deserialize)]
pub struct ContainersNamespacesListResponse {
	pub namespaces: Vec<ContainersNamespacesListResponseItem>,
}

#[derive(Deserialize)]
pub struct ContainersNamespacesListResponseItem {
	pub id: String,
	pub name: String,
	pub status: String,
}

#[derive(Deserialize)]
pub struct ContainersListResponse {
	pub containers: Vec<ContainersListResponseItem>,
}

#[derive(Deserialize, PartialEq, Eq, Clone)]
pub struct ContainersListResponseItem {
	pub id: String,
	pub name: String,
	pub domain_name: String,
}

impl ScalewayApi {
	pub fn new(api_key: &str, organization_id: &str, region: &str) -> Self {
		let client = reqwest::Client::builder()
			.user_agent("filen-relay-deployer")
			.default_headers({
				let mut headers = reqwest::header::HeaderMap::new();
				headers.insert(
					"X-Auth-Token",
					reqwest::header::HeaderValue::from_str(api_key).unwrap(),
				);
				headers
			})
			.build()
			.unwrap();
		Self {
			client,
			organization_id: organization_id.to_string(),
			region: region.to_string(),
		}
	}

	async fn request<T: for<'a> Deserialize<'a>, B: Serialize>(
		&self,
		method: Method,
		endpoint: &str,
		body: Option<&B>,
	) -> Result<T> {
		let url = format!(
			"https://api.scaleway.com/{}",
			endpoint.trim_start_matches('/')
		);
		log::debug!("Scaleway API: {} {}", method, url);
		if let Some(body) = body {
			log::debug!(
				"Request body: {}",
				serde_json::to_string(body)
					.as_deref()
					.unwrap_or("(request body could not be serialized)")
			);
		}
		let response = self.client.request(method, url);
		let response = if let Some(body) = body {
			response.json(body)
		} else {
			response
		};
		let response = response.send().await.context("Failed to send request")?;
		let text = response
			.text()
			.await
			.context("Failed to read response text")?;
		log::debug!("Response from {}: {}", endpoint, text);
		let result = serde_json::from_str::<T>(&text)
			.with_context(|| format!("Failed to parse JSON response (JSON: {})", text))?;
		Ok(result)
	}

	// ref: https://www.scaleway.com/en/developers/api

	pub async fn list_projects(&self) -> Result<Vec<ListProjectsResponseItem>> {
		let response = self
			.request::<ListProjectsResponse, _>(
				Method::GET,
				&format!(
					"account/v3/projects?organization_id={}",
					self.organization_id
				),
				None::<&()>,
			)
			.await?;
		Ok(response.projects)
	}

	pub async fn list_containers_namespaces(
		&self,
	) -> Result<Vec<ContainersNamespacesListResponseItem>> {
		let response = self
			.request::<ContainersNamespacesListResponse, _>(
				Method::GET,
				&format!("containers/v1beta1/regions/{}/namespaces", self.region),
				None::<&()>,
			)
			.await?;
		Ok(response.namespaces)
	}

	pub async fn create_containers_namespace(
		&self,
		name: &str,
		project_id: &str,
	) -> Result<ContainersNamespacesListResponseItem> {
		let response = self
			.request::<ContainersNamespacesListResponseItem, _>(
				Method::POST,
				&format!("containers/v1beta1/regions/{}/namespaces", self.region),
				Some(&serde_json::json!({
					"name": name,
					"project_id": project_id,
				})),
			)
			.await?;
		Ok(response)
	}

	pub async fn get_containers_namespace(
		&self,
		namespace_id: &str,
	) -> Result<ContainersNamespacesListResponseItem> {
		let response = self
			.request::<ContainersNamespacesListResponseItem, _>(
				Method::GET,
				&format!(
					"containers/v1beta1/regions/{}/namespaces/{}",
					self.region, namespace_id
				),
				None::<&()>,
			)
			.await?;
		Ok(response)
	}

	pub async fn list_containers(&self) -> Result<Vec<ContainersListResponseItem>> {
		let response = self
			.request::<ContainersListResponse, _>(
				Method::GET,
				&format!("containers/v1beta1/regions/{}/containers", self.region),
				None::<&()>,
			)
			.await?;
		Ok(response.containers)
	}

	pub async fn create_container(
		&self,
		body: &serde_json::Value,
	) -> Result<ContainersListResponseItem> {
		let response = self
			.request::<ContainersListResponseItem, _>(
				Method::POST,
				&format!("containers/v1beta1/regions/{}/containers", self.region),
				Some(body),
			)
			.await?;
		Ok(response)
	}

	pub async fn deploy_container(&self, container_id: &str) -> Result<()> {
		self.request::<serde_json::Value, _>(
			Method::POST,
			&format!(
				"containers/v1beta1/regions/{}/containers/{}/deploy",
				self.region, container_id
			),
			Some(&serde_json::json!({})),
		)
		.await?;
		Ok(())
	}

	pub async fn update_container(
		&self,
		container_id: &str,
		body: &serde_json::Value,
	) -> Result<()> {
		self.request::<serde_json::Value, _>(
			Method::PATCH,
			&format!(
				"containers/v1beta1/regions/{}/containers/{}",
				self.region, container_id
			),
			Some(body),
		)
		.await?;
		Ok(())
	}
}
