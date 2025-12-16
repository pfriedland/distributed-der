export HEADEND_GRPC=127.0.0.1:50070
export ASSETS_PATH=der_headend/assets_test.yaml
export GATEWAY_SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8

# Optional: logical gateway identifier; defaults to GATEWAY_SITE_ID.
# export GATEWAY_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8

cargo run -p edge_agent

