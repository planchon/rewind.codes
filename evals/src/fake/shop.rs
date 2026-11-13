//! `shop`: products and stock, purchase orders, sales and shipments, with typed results.
//!
//! `create_shipment` behaves like a real carrier's API: at most 2 shipments are created per
//! minute (more fail with `rate_limited` and `retry_after_s`), and the first request for
//! `O-603` times out after the shipment was created. A request repeated with the same
//! `request_id` returns the shipment it created; another `request_id` creates another
//! shipment.

use std::sync::Mutex;

use rewind_core::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Args, CallContext, Fixed, Reply, Service, ToolSpec, lock, object_schema, page};
use crate::clock::{DAY, MINUTE, parse_date, rfc3339};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Product {
    pub sku: String,
    pub name: String,
    pub stock: u64,
    pub reorder_point: u64,
    pub reorder_quantity: u64,
    pub supplier: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sale {
    pub id: String,
    pub date: String,
    pub region: String,
    pub sku: String,
    pub amount_cents: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PurchaseOrder {
    pub po_id: String,
    pub sku: String,
    pub quantity: u64,
    pub supplier: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shipment {
    pub shipment_id: String,
    pub order_id: String,
    pub request_id: String,
    pub created_at: String,
}

/// Shipments created per minute at most.
pub const SHIPMENTS_PER_MINUTE: usize = 2;
/// The order whose first shipment request times out after the shipment was created.
pub const TIMED_OUT_ORDER: &str = "O-603";

const ITEMS: [&str; 6] = [
    "Mug",
    "Desk lamp",
    "Notebook",
    "Backpack",
    "Cable",
    "Headset",
];
const SUPPLIERS: [&str; 4] = [
    "Globex Supply",
    "Initech Parts",
    "Acme Wholesale",
    "Kato & Co",
];

/// 60 products; one in seven is below its reorder point.
pub fn products() -> Vec<Product> {
    let mut fixed = Fixed::new(31);
    (1..=60u64)
        .map(|n| {
            let reorder_point = 10 + fixed.below(40);
            let stock = if n % 7 == 3 {
                fixed.below(reorder_point)
            } else {
                reorder_point + fixed.below(200)
            };
            Product {
                sku: format!("SKU-{n:03}"),
                name: format!("{} {}", fixed.pick(&ITEMS), n),
                stock,
                reorder_point,
                reorder_quantity: 25 * (1 + fixed.below(8)),
                supplier: fixed.pick(&SUPPLIERS).to_string(),
            }
        })
        .collect()
}

/// 1,500 sales over February 2026.
pub fn sales() -> Vec<Sale> {
    let mut fixed = Fixed::new(37);
    let february = parse_date("2026-02-01").unwrap_or(Timestamp(0));
    (1..=1_500u64)
        .map(|n| Sale {
            id: format!("S-{n:05}"),
            date: crate::clock::date_of(Timestamp(february.0 + fixed.below(28) * DAY)),
            region: fixed.pick(&["amer", "emea", "apac", "latam"]).to_string(),
            sku: format!("SKU-{:03}", 1 + fixed.below(60)),
            amount_cents: 199 + fixed.below(50_000),
        })
        .collect()
}

#[derive(Debug)]
struct State {
    products: Vec<Product>,
    purchase_orders: Vec<PurchaseOrder>,
    shipments: Vec<Shipment>,
    /// When each shipment was created, for the rate limit.
    created: Vec<Timestamp>,
    timed_out: bool,
}

#[derive(Debug)]
pub struct Shop {
    sales: Vec<Sale>,
    state: Mutex<State>,
}

impl Default for Shop {
    fn default() -> Self {
        Self::new()
    }
}

impl Shop {
    pub fn new() -> Self {
        Self {
            sales: sales(),
            state: Mutex::new(State {
                products: products(),
                purchase_orders: Vec::new(),
                shipments: Vec::new(),
                created: Vec::new(),
                timed_out: false,
            }),
        }
    }
}

fn product_schema() -> Value {
    object_schema(
        json!({
            "sku": { "type": "string" },
            "name": { "type": "string" },
            "stock": { "type": "integer" },
            "reorder_point": { "type": "integer", "description": "Reorder when the stock is below it." },
            "reorder_quantity": { "type": "integer", "description": "How many to order when reordering." },
            "supplier": { "type": "string" }
        }),
        &[
            "sku",
            "name",
            "stock",
            "reorder_point",
            "reorder_quantity",
            "supplier",
        ],
    )
}

impl Service for Shop {
    fn name(&self) -> &'static str {
        "shop"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "list_products",
                "List the products with their stock, 20 per page. Pass `next_cursor` back as `cursor` for the next page.",
                object_schema(json!({ "cursor": { "type": "string" } }), &[]),
            )
            .output(json!({
                "type": "object",
                "properties": { "products": { "type": "array", "items": product_schema() }, "next_cursor": { "type": "string" } },
                "required": ["products"]
            })),
            ToolSpec::new(
                "create_purchase_order",
                "Order stock of a product from a supplier.",
                object_schema(
                    json!({ "sku": { "type": "string" }, "quantity": { "type": "integer", "minimum": 1 }, "supplier": { "type": "string" } }),
                    &["sku", "quantity", "supplier"],
                ),
            )
            .output(object_schema(json!({ "po_id": { "type": "string" } }), &["po_id"])),
            ToolSpec::new(
                "list_sales",
                "The sales of a month (YYYY-MM), oldest first, up to `limit` per page (100 by default, 200 at most).",
                object_schema(
                    json!({
                        "month": { "type": "string", "pattern": "^[0-9]{4}-[0-9]{2}$" },
                        "cursor": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
                    }),
                    &["month"],
                ),
            )
            .output(json!({
                "type": "object",
                "properties": {
                    "sales": { "type": "array", "items": object_schema(json!({
                        "id": { "type": "string" },
                        "date": { "type": "string", "format": "date" },
                        "region": { "type": "string", "enum": ["amer", "emea", "apac", "latam"] },
                        "sku": { "type": "string" },
                        "amount_cents": { "type": "integer" }
                    }), &["id", "date", "region", "sku", "amount_cents"]) },
                    "next_cursor": { "type": "string" }
                },
                "required": ["sales"]
            })),
            ToolSpec::new(
                "create_shipment",
                "Ship an order. At most 2 shipments are created per minute: above, the call fails with `rate_limited` and `data.retry_after_s`. The carrier may time out (`timeout`) after creating the shipment: repeat the call with the same `request_id`, which returns the shipment already created instead of creating a second one.",
                object_schema(
                    json!({
                        "order_id": { "type": "string" },
                        "request_id": { "type": "string", "description": "Chosen by the caller, the same for every attempt at one shipment." }
                    }),
                    &["order_id", "request_id"],
                ),
            )
            .output(object_schema(
                json!({ "shipment_id": { "type": "string" }, "order_id": { "type": "string" }, "created_at": { "type": "string" } }),
                &["shipment_id", "order_id", "created_at"],
            )),
        ]
    }

    fn call(&self, context: &CallContext, tool: &str, args: &Map<String, Value>) -> Reply {
        let args = Args(args);
        let mut state = lock(&self.state);
        match tool {
            "list_products" => match page(&state.products, args.opt_str("cursor"), 20) {
                Ok((products, next)) => {
                    let mut result = json!({ "products": products });
                    if let Some(next) = next {
                        result["next_cursor"] = json!(next);
                    }
                    Reply::Structured(result)
                }
                Err(reply) => reply,
            },
            "create_purchase_order" => {
                let (sku, supplier) = match (args.str("sku"), args.str("supplier")) {
                    (Ok(sku), Ok(supplier)) => (sku.to_owned(), supplier.to_owned()),
                    (Err(reply), _) | (_, Err(reply)) => return reply,
                };
                let quantity = match args.u64("quantity") {
                    Ok(quantity) => quantity,
                    Err(reply) => return reply,
                };
                if !state.products.iter().any(|product| product.sku == sku) {
                    return Reply::error(format!("not_found: no product {sku}"));
                }
                let po_id = format!("PO-{}", state.purchase_orders.len() + 1);
                state.purchase_orders.push(PurchaseOrder {
                    po_id: po_id.clone(),
                    sku,
                    quantity,
                    supplier,
                });
                Reply::Structured(json!({ "po_id": po_id }))
            }
            "list_sales" => {
                let month = match args.str("month") {
                    Ok(month) => month,
                    Err(reply) => return reply,
                };
                let selected: Vec<&Sale> = self
                    .sales
                    .iter()
                    .filter(|sale| sale.date.starts_with(month))
                    .collect();
                let limit = args.opt_u64("limit").unwrap_or(100).clamp(1, 200) as usize;
                match page(&selected, args.opt_str("cursor"), limit) {
                    Ok((sales, next)) => {
                        let mut result = json!({ "sales": sales });
                        if let Some(next) = next {
                            result["next_cursor"] = json!(next);
                        }
                        Reply::Structured(result)
                    }
                    Err(reply) => reply,
                }
            }
            "create_shipment" => {
                let (order_id, request_id) = match (args.str("order_id"), args.str("request_id")) {
                    (Ok(order), Ok(request)) => (order.to_owned(), request.to_owned()),
                    (Err(reply), _) | (_, Err(reply)) => return reply,
                };
                if let Some(existing) = state
                    .shipments
                    .iter()
                    .find(|shipment| shipment.request_id == request_id)
                {
                    return Reply::Structured(json!({
                        "shipment_id": existing.shipment_id,
                        "order_id": existing.order_id,
                        "created_at": existing.created_at,
                    }));
                }
                let recent = state
                    .created
                    .iter()
                    .filter(|at| at.0 + MINUTE > context.now.0)
                    .count();
                if recent >= SHIPMENTS_PER_MINUTE {
                    return Reply::Error {
                        message: "rate_limited: at most 2 shipments per minute, retry after 60s"
                            .into(),
                        data: Some(json!({ "error": "rate_limited", "retry_after_s": 60 })),
                    };
                }
                let shipment = Shipment {
                    shipment_id: format!("SH-{}", state.shipments.len() + 1),
                    order_id,
                    request_id,
                    created_at: rfc3339(context.now),
                };
                state.created.push(context.now);
                state.shipments.push(shipment.clone());
                if shipment.order_id == TIMED_OUT_ORDER && !state.timed_out {
                    state.timed_out = true;
                    return Reply::Error {
                        message: "timeout: the carrier did not answer; the shipment may have been created, repeat the call with the same request_id".into(),
                        data: Some(json!({ "error": "timeout" })),
                    };
                }
                Reply::Structured(json!({
                    "shipment_id": shipment.shipment_id,
                    "order_id": shipment.order_id,
                    "created_at": shipment.created_at,
                }))
            }
            other => Reply::error(format!("unknown tool {other}")),
        }
    }

    fn snapshot(&self) -> Value {
        let state = lock(&self.state);
        json!({ "purchase_orders": state.purchase_orders, "shipments": state.shipments })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::T0;

    fn ship(shop: &Shop, at: u64, order: &str, request: &str) -> Reply {
        let context = CallContext {
            now: Timestamp(T0.0 + at),
            idempotency_key: None,
        };
        let args = json!({ "order_id": order, "request_id": request });
        shop.call(&context, "create_shipment", args.as_object().unwrap())
    }

    #[test]
    fn shipments_are_rate_limited_per_minute() {
        let shop = Shop::new();
        assert!(matches!(ship(&shop, 0, "O-601", "a"), Reply::Structured(_)));
        assert!(matches!(ship(&shop, 0, "O-602", "b"), Reply::Structured(_)));
        assert!(matches!(ship(&shop, 0, "O-604", "c"), Reply::Error { .. }));
        assert!(matches!(
            ship(&shop, MINUTE, "O-604", "c"),
            Reply::Structured(_)
        ));
    }

    #[test]
    fn a_timed_out_shipment_is_found_again_by_its_request_id() {
        let shop = Shop::new();
        assert!(matches!(
            ship(&shop, 0, TIMED_OUT_ORDER, "x"),
            Reply::Error { .. }
        ));
        assert!(matches!(
            ship(&shop, MINUTE, TIMED_OUT_ORDER, "x"),
            Reply::Structured(_)
        ));
        assert_eq!(shop.snapshot()["shipments"].as_array().unwrap().len(), 1);
        assert!(matches!(
            ship(&shop, MINUTE, TIMED_OUT_ORDER, "y"),
            Reply::Structured(_)
        ));
        assert_eq!(shop.snapshot()["shipments"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn fixtures_are_what_the_tasks_expect() {
        assert_eq!(
            products()
                .iter()
                .filter(|p| p.stock < p.reorder_point)
                .count(),
            9
        );
        assert!(sales().iter().all(|sale| sale.date.starts_with("2026-02")));
    }
}
