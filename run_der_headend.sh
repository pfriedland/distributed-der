export ASSETS_PATH=der_headend/assets_test.yaml

# Optional (DB-backed history):

export DATABASE_URL='postgres://bess:besspassword@localhost:5432/bess'
# Optional (dev reset): if DATABASE_URL is set and RESET_DB is truthy, tables are truncated on startup.
#export RESET_DB=1

# Pick a gRPC port and use it consistently. If you plan to use agent_launcher,
# setting HEADEND_GRPC_ADDR to 50070 avoids the common 50051 vs 50070 mismatch.
export HEADEND_GRPC_ADDR=127.0.0.1:50070

cargo run -p der_headend
