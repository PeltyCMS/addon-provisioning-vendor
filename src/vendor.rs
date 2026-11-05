//! Resell third-party services (SSL, email, backup, site builder, security…)
//! through one vendor REST API, the way WHMCS MarketConnect does.
//!
//! The vendor publishes a catalog with the reseller's cost; the host syncs it
//! (`catalog` op), staff import items as products with a markup, and orders
//! are fulfilled with the normal module lifecycle.
//!
//! Contract (`/api/v1`, Bearer api key):
//! - `GET /account`, connection test
//! - `GET /catalog` → `{"products": [{sku, name, description, cost, currency, cycle}]}`
//! - `POST /orders` `{sku, quantity, domain, customer{email,first_name,last_name}, external_id}` → `{id, status}`
//! - `POST /orders/{id}/{suspend|unsuspend|cancel}`, `POST /orders/{id}/change` `{sku}`
//! - `GET /orders/{id}` (custom `status`), `POST /orders/{id}/login` → `{url}` (SSO)
//!
//! No real vendor is wired in; behaviour is verified against a local mock.
use pelty_provisioning::http::{call, Method};
use pelty_provisioning::{
    cfg_str, cfg_u64_or, fld, remote_id, BoxFuture, Created, ModuleInfo, ProvisionError,
    ProvisionResult, ProvisioningModule, ServerConn, ServiceCtx, VendorProduct,
};
use serde_json::{json, Value};

const J: &str = "application/json";

#[derive(Default)]
pub struct VendorReseller;

fn oid(svc: &ServiceCtx) -> ProvisionResult<&str> {
    let id = remote_id(svc)?;
    if !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(id)
    } else {
        Err(ProvisionError::Config("invalid order id".into()))
    }
}

fn sku_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Parse a vendor catalog body (pure; unit-tested). Invalid rows are skipped.
pub fn parse_catalog(v: &Value) -> Vec<VendorProduct> {
    let rows = v
        .get("products")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    rows.iter()
        .filter_map(|r| {
            let sku = text(&r["sku"])?;
            let cost = text(&r["cost"])?;
            let currency = text(&r["currency"])?.to_ascii_uppercase();
            if !sku_ok(&sku)
                || currency.len() != 3
                || !currency.chars().all(|c| c.is_ascii_uppercase())
            {
                return None;
            }
            // Decimal string only: digits with at most one dot, no sign/exponent.
            if cost.is_empty()
                || cost.matches('.').count() > 1
                || !cost.chars().all(|c| c.is_ascii_digit() || c == '.')
            {
                return None;
            }
            Some(VendorProduct {
                name: text(&r["name"])
                    .unwrap_or_else(|| sku.clone())
                    .chars()
                    .take(200)
                    .collect(),
                description: text(&r["description"])
                    .unwrap_or_default()
                    .chars()
                    .take(5000)
                    .collect(),
                cycle: text(&r["cycle"]).unwrap_or_else(|| "annual".into()),
                product_config: json!({ "sku": sku }),
                sku,
                cost,
                currency,
            })
        })
        .collect()
}

impl ProvisioningModule for VendorReseller {
    fn info(&self) -> ModuleInfo {
        ModuleInfo {
            id: "vendor_reseller",
            name: "Vendor reseller (MarketConnect-style)",
            server_fields: vec![],
            product_fields: vec![
                fld!("sku", "text", true),
                fld!("quantity", "number", false, 1),
            ],
            actions: vec![],
        }
    }
    fn custom_actions(&self) -> &'static [&'static str] {
        &["status"]
    }
    fn test_connection<'a>(&'a self, server: &'a ServerConn) -> BoxFuture<'a, ProvisionResult<()>> {
        Box::pin(async move {
            call(server, Method::GET, "/api/v1/account", J, None)
                .await
                .map(|_| ())
        })
    }
    fn catalog<'a>(
        &'a self,
        server: &'a ServerConn,
    ) -> BoxFuture<'a, ProvisionResult<Option<Vec<VendorProduct>>>> {
        Box::pin(async move {
            Ok(Some(parse_catalog(
                &call(server, Method::GET, "/api/v1/catalog", J, None).await?,
            )))
        })
    }
    fn create<'a>(
        &'a self,
        server: &'a ServerConn,
        svc: &'a ServiceCtx,
    ) -> BoxFuture<'a, ProvisionResult<Created>> {
        Box::pin(async move {
            if let Some(id) = &svc.remote_id {
                return Ok(Created {
                    remote_id: id.clone(),
                    module_data: svc.module_data.clone(),
                });
            }
            let sku = cfg_str(&svc.product_config, "sku")?;
            if !sku_ok(sku) {
                return Err(ProvisionError::Config("invalid sku".into()));
            }
            let domain = svc
                .custom_fields
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or("");
            let body = json!({
                "sku": sku, "quantity": cfg_u64_or(&svc.product_config, "quantity", 1)?, "domain": domain,
                "customer": { "email": svc.customer_email, "first_name": svc.customer_first_name, "last_name": svc.customer_last_name },
                // Idempotency on the vendor side: a retried create maps to the same order.
                "external_id": format!("pelty-{}", svc.service_id),
            });
            let r = call(server, Method::POST, "/api/v1/orders", J, Some(&body)).await?;
            let id = text(&r["id"])
                .ok_or_else(|| ProvisionError::Transient("vendor returned no order id".into()))?;
            Ok(Created {
                remote_id: id,
                module_data: json!({ "sku": sku, "status": r.get("status") }),
            })
        })
    }
    fn suspend<'a>(
        &'a self,
        server: &'a ServerConn,
        svc: &'a ServiceCtx,
    ) -> BoxFuture<'a, ProvisionResult<()>> {
        Box::pin(async move {
            call(
                server,
                Method::POST,
                &format!("/api/v1/orders/{}/suspend", oid(svc)?),
                J,
                None,
            )
            .await
            .map(|_| ())
        })
    }
    fn unsuspend<'a>(
        &'a self,
        server: &'a ServerConn,
        svc: &'a ServiceCtx,
    ) -> BoxFuture<'a, ProvisionResult<()>> {
        Box::pin(async move {
            call(
                server,
                Method::POST,
                &format!("/api/v1/orders/{}/unsuspend", oid(svc)?),
                J,
                None,
            )
            .await
            .map(|_| ())
        })
    }
    fn terminate<'a>(
        &'a self,
        server: &'a ServerConn,
        svc: &'a ServiceCtx,
    ) -> BoxFuture<'a, ProvisionResult<()>> {
        Box::pin(async move {
            pelty_provisioning::util::gone_ok(
                call(
                    server,
                    Method::POST,
                    &format!("/api/v1/orders/{}/cancel", oid(svc)?),
                    J,
                    None,
                )
                .await,
            )
        })
    }
    fn change_package<'a>(
        &'a self,
        server: &'a ServerConn,
        svc: &'a ServiceCtx,
    ) -> BoxFuture<'a, ProvisionResult<()>> {
        Box::pin(async move {
            let sku = cfg_str(&svc.product_config, "sku")?;
            call(
                server,
                Method::POST,
                &format!("/api/v1/orders/{}/change", oid(svc)?),
                J,
                Some(&json!({ "sku": sku })),
            )
            .await
            .map(|_| ())
        })
    }
    fn sso_url<'a>(
        &'a self,
        server: &'a ServerConn,
        svc: &'a ServiceCtx,
        _token: &'a str,
    ) -> BoxFuture<'a, ProvisionResult<Option<String>>> {
        Box::pin(async move {
            let r = call(
                server,
                Method::POST,
                &format!("/api/v1/orders/{}/login", oid(svc)?),
                J,
                None,
            )
            .await?;
            Ok(r["url"]
                .as_str()
                .filter(|u| u.starts_with("https://"))
                .map(str::to_string))
        })
    }
    fn custom<'a>(
        &'a self,
        server: &'a ServerConn,
        svc: &'a ServiceCtx,
        action: &'a str,
        _params: &'a Value,
    ) -> BoxFuture<'a, ProvisionResult<Value>> {
        Box::pin(async move {
            match action {
                "status" => {
                    let r = call(
                        server,
                        Method::GET,
                        &format!("/api/v1/orders/{}", oid(svc)?),
                        J,
                        None,
                    )
                    .await?;
                    Ok(
                        json!({ "status": r.get("status"), "module_data_patch": { "status": r.get("status") } }),
                    )
                }
                _ => Err(ProvisionError::Config(format!("unknown action {action}"))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_rows_are_validated() {
        let v = json!({"products": [
            {"sku": "ssl-dv", "name": "DV SSL", "cost": "4.50", "currency": "usd", "cycle": "annual"},
            {"sku": "mail-pro", "cost": 2, "currency": "EUR", "cycle": "monthly"},
            {"sku": "bad sku", "cost": "1", "currency": "EUR"},
            {"sku": "neg", "cost": "-1", "currency": "EUR"},
            {"sku": "exp", "cost": "1e3", "currency": "EUR"},
            {"sku": "cur", "cost": "1", "currency": "EURO"},
            {"name": "no sku", "cost": "1", "currency": "EUR"}
        ]});
        let c = parse_catalog(&v);
        assert_eq!(c.len(), 2);
        assert_eq!(
            (
                c[0].sku.as_str(),
                c[0].currency.as_str(),
                c[0].cost.as_str()
            ),
            ("ssl-dv", "USD", "4.50")
        );
        assert_eq!(
            (
                c[1].name.as_str(),
                c[1].cost.as_str(),
                c[1].product_config["sku"].as_str()
            ),
            ("mail-pro", "2", Some("mail-pro"))
        );
        assert!(parse_catalog(&json!({})).is_empty());
    }
}
