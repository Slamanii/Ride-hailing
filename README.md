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

## Quick start (local)
1. Inspect files in this zip.
2. Run `bash scripts/setup.sh` to see suggested commands for environment setup.
3. For Anchor program: install Rust + Solana toolchain (see Anchor docs).
4. For web: run `cd web && npm install && npm run dev` (Next.js dev)
5. For mobile: `cd mobile && npm install && expo start`

This scaffold is minimal and intended as a starting point. Expand components, integrate wallet adapters, and wire up the Anchor program as needed.


## Backend API Usage Guide (Async Endpoints)

This backend exposes async HTTP endpoints that the frontend can call to create trips, , create rider accounts, create driver account, create ride request, update trips, notify drivers, and query trip status.
All endpoints return JSON and are designed to be consumed by a web or mobile frontend.

## Backend URL
``
http://localhost:8080
``

## Authentication

Currently, endpoints do not enforce authentication.
Future versions may require:

- JWT Authorization header

- Wallet signature

- API key

## 1. Admin Dashboard

```http
GET /admin/admin-dashboard

```

## Description
Returns an accurate real time record from the database of drivers and riders who have data stored in the DB.

## Example Request Body for drivers & riders
```json
{
  "rider_id": "uuid-string",
  "pick_up": "Pickup location",
  "drop_off": "Dropoff location",
  "driver_location": "Driver current location",
  "rider_pubkey": "solana_pubkey_string",
  "driver_pubkey": "solana_pubkey_string",
  "driver_id": "uuid-string",
  "distance_km": 12.4,
  "item": {
    "type": "package",
    "weight": "2kg"
  },
  "fare_estimate": 4500,
  "rider_email": "user@email.com"
}

```


## 2. Get Riders Count

```http
GET /admin/get-riders

```

## Description
Returns a record count of 20 rider accounts at a time from the database of riders who have data stored in the DB.




## 3. Create Rider Account

```http
POST /admin/create-riders

```

## Description
This endpoint creates a new rider account in db using new rider request that takes a json called Rider Request from the frontend or mobile.




## 4. Get Drivers Count

```http
GET /admin/get-drivers

```

## Description
Returns a record count of 20 driver accounts at a time from the database of drivers who have data stored in the DB.



## 5. Create Driver Account

```http
POST /admin/create-drivers

```

## Description
This endpoint creates a new driver account in db using new driver request that takes a json called Driver Request from the frontend or mobile.



## 6. Notify Driver Handler

```http
GET /drivers/notify-driver

```

## Description
This endpoint creates a new connection between the driver frontend app or mobile and rider frontend app by returning the every new ride request to a certain driver account if requirements are met.

## Example Ride Request Body
```json
{
  "rider_id": "uuid-string",
  "pick_up": "Pickup location",
  "drop_off": "Dropoff location",
  "driver_location": "Driver current location",
  "rider_pubkey": "solana_pubkey_string",
  "driver_pubkey": "solana_pubkey_string",
  "driver_id": "uuid-string",
  "distance_km": 12.4,
  "item": {
    "type": "package",
    "weight": "2kg"
  },
  "fare_estimate": 4500,
  "rider_email": "user@email.com"
}

```

## 7. Driver Response Handler

```http
GET /drivers/driver-response

```

## Description
This endpoint checks first that a driver account is not actively running more than the allowed ongoing trips and is off the expected vehicle type, if the account passes the constraints the endpoint then takes the driver repsonse payload json, a parameter that is expected to expose response option to the driver frontend app.

Note: Choosing to handle checks for ongoing trips and vehicle type match is this function might later be found to lead to drivers receiving notifications for trips without option to accept or decline because they do not pass the constraints, with this said it is important to ensure that the ednpoint is not exposed before the notify driver endpoint. 

## Example Driver Response Json
```json

{
    "driver_response":  { 
                      "response":  "Accepted",
                      "response": "Rejected",
                        } 
    
}

```


## 8. Update Drivers Account

```http
POST /drivers/update-driver

```

## Description
This function updates the driver_location & driver_reponse fields of its DB struct. reference the Driver Response Enum in drivera.rs for more info on what it represents.



## 9. Assign Driver Handler

```http
GET /riders/assign-driver

```

## Description
This endpoint checks first that a driver is within required distance of the pick up location and is off the expected vehicle type, if the account passes the constraints the endpoint then creates a channel between the notify-driver handler(alerting the driver frontend app that there is a match) and wait-driver-response(returning the response from Driver_Response_handler via the DriverResponsePayloadOut json).
The success of this endpoint returns a struct in the form of json type called RideAssignment.

Note: The process will repeat itself 4 times after which it will timeout and the user of the frontend will have to send a new request to proceed.




## 10. Driver Response Payload Out handler

```http
GET /riders/driver-response

```

## Description
This endpoint recieves the json DriverResponsePayloadOut from the driver-response handler that takes the DriverResponsePayload json. the main idea here is recieving the response from the driver frontend after been notified of the ride request and return it back to assign_driver to return ride assigned successfully.


## 11. Create Ride Request & store in DB

```http
POST /riders/ride-request

```

## Description
This endpoint creates a new ride request struct in db using new request_ride function that takes a json called CreateRideRequest from the frontend or mobile.



## 12. Create Trip & store in DB

```http
POST /trips/create-trip

```

## Description
This endpoint creates a new Trip struct in db using new create_trip function that takes a json called CreateTripInput from the backend.
Exposing this endpoint is neccessary for specifying ongoing trips in the rides section of the riders frontend app, I believe the driver frontend app might expose this as well.




## 13. Update Trips stored in DB

```http
POST /trips/update-trip

```

## Description
This function updates the status, end_ts, fare_estimate, fare_lamports fields of the trips DB struct. Exposing this endpoint is neccessary for specifying completed trips in the rides section of the riders frontend app, the driver frontend app should expose this as well.




## 14. Get Trip by Reference

```http
GET /trips/get-trip{reference}

```

## Description
Fetches a single trip from the database using its reference ID.
This will be used mostly in escrow.rs & paystack, exposing for the frontend might be neccessary not sure yet I kept it async so it would be available if needed.

## Example Success Response
```json
{
  "trip_id": "base64-or-bytes",
  "rider_id": "uuid",
  "driver_id": "uuid",
  "status": "Ongoing",
  "start_ts": 1712345678,
  "end_ts": null,
  "fare_estimate": 4500,
  "fare_lamports": null
}
```

## 15. Process Geolocation Data

```http
POST /matching/process-geolocation

```

## Description
The primary purpose of this end point is to use GeoPointRequest data gotten in json form from the frontend map api to update fields that implement GeoPoint type, you would most likely have to go to matching.rs to read the code for better understanding or copy into prefered agent for clarity.

Note: This is critical as returing inaccurate data to GeoPointRequest struct from the map api would lead to inaccurate matching and that would be catastrophic. 




## 16. Create Paystack Transcation 

```http
POST /paystack/create-transaction{reference}

```

## Description
This endpoint helps us to create paystack transactions that expose payment information from the paysatck handler to our riders frontend app allowing us to recieve payments after ride has been completed. It then triggers a second onchain transcation that is constructed from trip data in the db allowing us to log completed rides permanently reducing monthly database maintanance cost.




## 17. Paystack Payment Confirmation Handler 

```http
POST /escrow/api/paystack/webhook

```

## Description
This endpoint helps us to handle paystack transaction confirmation,  It is used to build a transcation in rust for solana_client allowing us to log completed rides permanently onchain.


Note: I havent implemented paystack split. It ensures money goes to the treasury who inturn pays rent for creating trips onchain.

## Succes Response
```http
200 OK

```

## Json Response Example
```json
"Trip Created"

```

## Failure Response Example
```http
500 Internal server Error
```

## Json Response Example
```json
"Error: Database query failed"
```


## Important Notice
## Frontend → Backend JSON Data Contracts

This backend relies on direct JSON deserialization into Rust structs to construct business logic.
These JSON objects must match the Rust struct shape exactly so that serde can deserialize them without errors.

These are NOT HTTP endpoint definitions.
They are data schemas used internally by the backend when processing requests.

## General Rules (IMPORTANT)

- All JSON keys are snake_case

- All required fields must be present

- Types must match exactly (string ≠ number)

- Missing or mismatched fields will cause deserialization failure

Example

## Rider Request Json
```json
{
  "name": "Jane Doe",
  "email": "jane@example.com",
  "phone": "08012345678",
  "rider_pubkey": "SolanaPublicKeyString"
}
```

```Rust
struct RiderRequest {
    name: String,
    email: String,
    phone: String,
    rider_pubkey: String
}
```
Look through the codebase to find the target structs;

- RiderRequest
- DriverRequest
- CreateRideRequest
- GeoPointRequest












