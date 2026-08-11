//! Minimal GraphQL server for trying out `examples/graphql.test.yaml`:
//!
//! ```sh
//! cargo run -p perfscale-core --example graphql_server
//! # in another terminal:
//! cargo run -p perfscale-cli -- run -f examples/graphql.test.yaml
//! ```
//!
//! Serves 127.0.0.1:4000/graphql with introspection enabled (async-graphql
//! default), so the `std/graphql@v1` steps exercise the full
//! introspection → validate → execute path. Dev tool only.

use async_graphql::{EmptySubscription, Object, Schema, SimpleObject};

#[derive(SimpleObject, Clone)]
struct Viewer {
    id: String,
    name: String,
}

#[derive(SimpleObject)]
struct Widget {
    id: String,
    name: String,
}

struct Query;

#[Object]
impl Query {
    async fn viewer(&self, id: Option<String>) -> Viewer {
        Viewer {
            id: id.unwrap_or_else(|| "u-1".into()),
            name: "Ada".into(),
        }
    }

    async fn widgets(&self) -> Vec<Widget> {
        (1..=3)
            .map(|i| Widget {
                id: format!("w-{i}"),
                name: format!("widget-{i}"),
            })
            .collect()
    }
}

struct Mutation;

#[Object]
impl Mutation {
    async fn rename_widget(&self, id: String, name: String) -> Widget {
        Widget { id, name }
    }
}

#[tokio::main]
async fn main() {
    let schema = Schema::build(Query, Mutation, EmptySubscription).finish();
    let app = axum::Router::new().route(
        "/graphql",
        axum::routing::get_service(async_graphql_axum::GraphQL::new(schema.clone()))
            .post_service(async_graphql_axum::GraphQL::new(schema)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:4000")
        .await
        .expect("bind 127.0.0.1:4000");
    println!("GraphQL example server on http://127.0.0.1:4000/graphql");
    axum::serve(listener, app).await.expect("serve");
}
