# WebSocket flow (starter notes)

This file outlines a simple WebSocket flow for driver-user real-time communication.

1. Client (user) opens WS connection to /ws/user?userId=...
2. Driver opens WS connection to /ws/driver?driverId=...
3. A lightweight server (Node/Express + ws or Socket.IO) maintains mapping:
   - userId -> socket
   - driverId -> socket
4. When a user requests a ride:
   - client -> server: POST /request {pickup, dropoff, userId}
   - server finds nearby drivers, forwards request via WS to driver sockets
5. Driver accepts:
   - driver -> server via WS: {type: "accept", rideId, driverId}
   - server updates DB and forwards confirmation to user socket
6. Use JWT for authentication on initial WS handshake and reconnect logic.

See `websocket-server-starter.js` for a minimal Node example (not included).
