use kube::api::{Api, PatchParams, Patch};
use kube::Client;
use kube::ResourceExt;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use k8s_openapi::chrono::Utc;

use gateway_api::gateways::Gateway;
use gateway_api::gatewayclasses::GatewayClass;

use crate::logging::{debug, warn};

pub async fn update_gateway_status(
    client: &Client,
    gateway: &Gateway,
    programmed: bool,
    message: &str,
) {
    let name = gateway.name_any();
    let ns = gateway.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<Gateway> = Api::namespaced(client.clone(), &ns);

    let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());
    let generation = gateway.metadata.generation;

    let conditions = vec![
        Condition {
            type_: "Accepted".to_string(),
            status: "True".to_string(),
            reason: "Accepted".to_string(),
            message: "Gateway accepted by uringress controller".to_string(),
            last_transition_time: now.clone(),
            observed_generation: generation,
        },
        Condition {
            type_: "Programmed".to_string(),
            status: if programmed { "True" } else { "False" }.to_string(),
            reason: if programmed { "Programmed" } else { "Invalid" }.to_string(),
            message: message.to_string(),
            last_transition_time: now,
            observed_generation: generation,
        },
    ];

    let status = serde_json::json!({
        "status": {
            "conditions": conditions,
        }
    });

    match api
        .patch_status(&name, &PatchParams::apply("uringress"), &Patch::Merge(status))
        .await
    {
        Ok(_) => debug!("Updated Gateway {}/{} status", ns, name),
        Err(e) => warn!("Failed to update Gateway {}/{} status: {}", ns, name, e),
    }
}

pub async fn update_gateway_class_status(
    client: &Client,
    gc: &GatewayClass,
    accepted: bool,
    message: &str,
) {
    let name = gc.name_any();
    let api: Api<GatewayClass> = Api::all(client.clone());

    let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());
    let generation = gc.metadata.generation;

    let conditions = vec![Condition {
        type_: "Accepted".to_string(),
        status: if accepted { "True" } else { "False" }.to_string(),
        reason: if accepted { "Accepted" } else { "InvalidParameters" }.to_string(),
        message: message.to_string(),
        last_transition_time: now,
        observed_generation: generation,
    }];

    let status = serde_json::json!({
        "status": {
            "conditions": conditions,
        }
    });

    match api
        .patch_status(&name, &PatchParams::apply("uringress"), &Patch::Merge(status))
        .await
    {
        Ok(_) => debug!("Updated GatewayClass {} status", name),
        Err(e) => warn!("Failed to update GatewayClass {} status: {}", name, e),
    }
}
