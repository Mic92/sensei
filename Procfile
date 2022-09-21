bitcoind: bitcoind  -fallbackfee=0.0002 -server -rpcuser=polaruser -rpcpassword=polarpass -chain=regtest
sensei: bash ./setup-sensei && cargo run --bin senseid -- \
  --network=regtest --bitcoind-rpc-host=localhost --bitcoind-rpc-port=18443 --bitcoind-rpc-username=polaruser --bitcoind-rpc-password=polarpass --database-url=sensei.db
fund-wallet: bash ./setup-wallet && sleep 99999999
