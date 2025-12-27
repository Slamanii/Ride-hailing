# ASAP Ride-hailing Scaffold

This scaffold was generated for you. It includes:
- Anchor Solana program starter (`programs/ride_program`)
- `web/` Next.js + TypeScript starter with Tailwind placeholders
- `mobile/` Expo React Native TypeScript starter with an Account screen
- Monorepo `package.json` using npm workspaces
- Basic TypeScript Anchor test
- WebSocket flow doc and setup script

Defaults used:
- TypeScript across web & mobile
- Expo for React Native
- Anchor (Rust) for Solana program
- npm workspaces

## ASAP Logic (Backend API)

This folder contains the backend HTTP API for the ASAP platform, built with Actix-web.

Frontend applications (web & mobile) interact with this service only via HTTP endpoints.

# How Routing Is Structured

Routes are not defined in main.rs.

Instead, routing is registered through Actix ServiceConfig functions exposed by the api and services folders.

# Actual Server Setup (Important)

main.rs starts the server like this:

```rust
HttpServer::new(move || {
    App::new()
        .app_data(web::Data::new(pool.clone()))
        .configure(api::init)
        .configure(services::init)
})
.bind(("127.0.0.1", 8080))?
.run()
.await
```

This means:

. api::init registers all API routes

. services::init registers all service routes

. main.rs does no routing logic itself

## Folder Responsibilities
```css
logic/
 └── src/
     ├── api/
     │   ├── riders/
     │   ├── trips/
     │   ├── drivers/
     │   ├── admin/
     │   └── mod.rs
     │
     ├── services/
     │   ├── matching/
     │   ├── escrow/
     │   ├── pricing/
     │   ├── paystack/
     │   └── mod.rs
     │
     ├── db/
     │   └── pool.rs   (no HTTP routes)
     └── main.rs
```

## API Folder (api/)

Contains HTTP endpoints exposed to frontend apps

Organized by feature (riders, trips, drivers, admin)

Each module defines routes using web::scope

Example:

```bash
/riders/*
/drivers/*
/trips/*
```

api/mod.rs exposes a single entry point:

```rust
pub fn init(cfg: &mut ServiceConfig) {
    riders::init(cfg);
    trips::init(cfg);
    drivers::init(cfg);
    admin::init(cfg);
}
```

## Services Folder (services/)

. Contains service-level endpoints and logic

. Used for flows like:

. Driver matching

. Pricing

. Escrow

. Payments (Paystack)

Routes are also registered via:

```rust
pub fn init(cfg: &mut ServiceConfig) {
    matching::init(cfg);
    pricing::init(cfg);
    escrow::init(cfg);
    paystack::init(cfg);
}
```

## Database Layer (db)

. Contains database connection pooling and helpers

. No HTTP endpoints

. Frontend never interacts with this layer directly

## What Frontend Devs Need to Know

. Frontend does not call Rust functions

. Frontend calls HTTP endpoints

. Routes are grouped by feature and automatically registered at startup

. All endpoints are active once the server is running

Example calls:

```bash
POST /matching/request
GET  /drivers/nearby
POST /trips/create
```

Each endpoint maps internally to an async handler.

## Async Behavior

Some endpoints may:

. Wait for driver responses

. Perform matching logic

. Interact with pricing or escrow services

Frontend should handle:

. Loading states

. Timeouts

. Error responses (non-200)

## Summary

. main.rs only wires things together

. Routing lives in api::init and services::init

. Each folder owns its own endpoints

. Frontend communicates via HTTP only
