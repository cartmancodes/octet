A P-256 key for tests only (crates/octet-hub/src/apns.rs), made with openssl.
It signs nothing real and is not anyone's APNs key. The tests copy the PEM
block below into a private 0600 file before the hub reads it.

-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg6gtm8Rz/4NTupBrH
5kqV/1Tb4Szps3DEOMylwKr2e3KhRANCAAQNyklOrsaSogNrkqtUhOLzT4qalWuq
RFf3yfkSF5uuEU3FBKnk2ZzOyYlZCb4fwL5G4VtCoZXLvtX9f+N5fWTp
-----END PRIVATE KEY-----
