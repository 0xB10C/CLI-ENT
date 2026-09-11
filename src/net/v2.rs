//! v2 (BIP324) transport: drives bip324's sans-IO `Handshake` over the raw
//! `TcpStream`, keeps the cipher pair, and hand-encodes the short-ID message form
//! (there is no `V2NetworkMessage` in released bitcoin 0.32). PLAN.md §5.
//! Implemented in milestone 3.
