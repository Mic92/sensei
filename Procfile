# TODO for now we use polar to setup the network
#bitcoind: bitcoind  -fallbackfee=0.0002 -server -rpcuser=polaruser -rpcpassword=polarpass -chain=regtest
#fund-wallet: bash ./setup-wallet && sleep 99999999
sensei: bash ./setup-sensei && cargo run --bin senseid -- --network=regtest --bitcoind-rpc-host=localhost --bitcoind-rpc-port=18446 --bitcoind-rpc-username=polaruser --bitcoind-rpc-password=polarpass --database-url=sensei.db
polar: polar
