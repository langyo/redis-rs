#![cfg(feature = "cluster-async")]
mod support;

#[cfg(test)]
mod cluster_async {
    use std::{
        collections::HashMap,
        sync::{
            atomic::{self, AtomicBool, AtomicI32, AtomicU16, AtomicU32, Ordering},
            Arc,
        },
        time::Duration,
    };

    use futures::prelude::*;
    use futures_time::{future::FutureExt, task::sleep};
    use once_cell::sync::Lazy;

    use redis::{
        aio::{ConnectionLike, MultiplexedConnection},
        cluster::ClusterClient,
        cluster_async::Connect,
        cluster_routing::{MultipleNodeRoutingInfo, RoutingInfo, SingleNodeRoutingInfo},
        cmd, from_owned_redis_value, parse_redis_value, pipe, AsyncCommands, Cmd, ErrorKind,
        InfoDict, IntoConnectionInfo, ProtocolVersion, RedisError, RedisFuture, RedisResult,
        Script, Value,
    };
    use redis_test::cluster::{RedisCluster, RedisClusterConfiguration};
    use redis_test::server::use_protocol;
    use rstest::rstest;

    use crate::support::*;

    fn broken_pipe_error() -> RedisError {
        RedisError::from(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "mock-io-error",
        ))
    }

    async fn smoke_test_connection(mut connection: impl redis::aio::ConnectionLike) {
        cmd("SET")
            .arg("test")
            .arg("test_data")
            .exec_async(&mut connection)
            .await
            .expect("SET command should succeed");
        let res: String = cmd("GET")
            .arg("test")
            .clone()
            .query_async(&mut connection)
            .await
            .expect("GET command should succeed");
        assert_eq!(res, "test_data");
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_basic_cmd(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new();

        block_on_all(
            async move {
                let connection = cluster.async_connection().await;
                smoke_test_connection(connection).await;
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_basic_eval(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new();

        block_on_all(
            async move {
                let mut connection = cluster.async_connection().await;
                let res: String = cmd("EVAL")
                    .arg(
                        r#"redis.call("SET", KEYS[1], ARGV[1]); return redis.call("GET", KEYS[1])"#,
                    )
                    .arg(1)
                    .arg("key")
                    .arg("test")
                    .query_async(&mut connection)
                    .await?;
                assert_eq!(res, "test");
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_basic_script(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new();

        block_on_all(
            async move {
                let mut connection = cluster.async_connection().await;
                let res: String = Script::new(
                    r#"redis.call("SET", KEYS[1], ARGV[1]); return redis.call("GET", KEYS[1])"#,
                )
                .key("key")
                .arg("test")
                .invoke_async(&mut connection)
                .await?;
                assert_eq!(res, "test");
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_route_flush_to_specific_node(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new();

        block_on_all(
            async move {
                let mut connection = cluster.async_connection().await;
                let _: () = connection.set("foo", "bar").await.unwrap();
                let _: () = connection.set("bar", "foo").await.unwrap();

                let res: String = connection.get("foo").await.unwrap();
                assert_eq!(res, "bar".to_string());
                let res2: Option<String> = connection.get("bar").await.unwrap();
                assert_eq!(res2, Some("foo".to_string()));

                let route =
                    redis::cluster_routing::Route::new(1, redis::cluster_routing::SlotAddr::Master);
                let single_node_route =
                    redis::cluster_routing::SingleNodeRoutingInfo::SpecificNode(route);
                let routing = RoutingInfo::SingleNode(single_node_route);
                assert_eq!(
                    connection
                        .route_command(&redis::cmd("FLUSHALL"), routing)
                        .await
                        .unwrap(),
                    Value::Okay
                );
                let res: String = connection.get("foo").await.unwrap();
                assert_eq!(res, "bar".to_string());
                let res2: Option<String> = connection.get("bar").await.unwrap();
                assert_eq!(res2, None);
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_route_flush_to_node_by_address(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new();

        block_on_all(
            async move {
                let mut connection = cluster.async_connection().await;
                let mut cmd = redis::cmd("INFO");
                // The other sections change with time.
                // TODO - after we remove support of redis 6, we can add more than a single section - .arg("Persistence").arg("Memory").arg("Replication")
                cmd.arg("Clients");
                let value = connection
                    .route_command(
                        &cmd,
                        RoutingInfo::MultiNode((MultipleNodeRoutingInfo::AllNodes, None)),
                    )
                    .await
                    .unwrap();

                let info_by_address =
                    from_owned_redis_value::<HashMap<String, String>>(value).unwrap();
                // find the info of the first returned node
                let (address, info) = info_by_address.into_iter().next().unwrap();
                let mut split_address = address.split(':');
                let host = split_address.next().unwrap().to_string();
                let port = split_address.next().unwrap().parse().unwrap();

                let value = connection
                    .route_command(
                        &cmd,
                        RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress { host, port }),
                    )
                    .await
                    .unwrap();
                let new_info = from_owned_redis_value::<String>(value).unwrap();

                assert_eq!(new_info, info);
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_route_info_to_nodes(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new_with_config(RedisClusterConfiguration {
            num_nodes: 12,
            num_replicas: 1,
            ..Default::default()
        });

        let split_to_addresses_and_info = |res| -> (Vec<String>, Vec<String>) {
            if let Value::Map(values) = res {
                let mut pairs: Vec<_> = values
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            redis::from_redis_value::<String>(&key).unwrap(),
                            redis::from_redis_value::<String>(&value).unwrap(),
                        )
                    })
                    .collect();
                pairs.sort_by(|(address1, _), (address2, _)| address1.cmp(address2));
                pairs.into_iter().unzip()
            } else {
                unreachable!("{:?}", res);
            }
        };

        block_on_all(
            async move {
                let cluster_addresses: Vec<_> = cluster
                    .cluster
                    .servers
                    .iter()
                    .map(|server| server.connection_info())
                    .collect();
                let client = ClusterClient::builder(cluster_addresses.clone())
                    .read_from_replicas()
                    .build()?;
                let mut connection = client.get_async_connection().await?;

                let route_to_all_nodes = redis::cluster_routing::MultipleNodeRoutingInfo::AllNodes;
                let routing = RoutingInfo::MultiNode((route_to_all_nodes, None));
                let res = connection
                    .route_command(&redis::cmd("INFO"), routing)
                    .await
                    .unwrap();
                let (addresses, infos) = split_to_addresses_and_info(res);

                let mut cluster_addresses: Vec<_> = cluster_addresses
                    .into_iter()
                    .map(|info| info.addr.to_string())
                    .collect();
                cluster_addresses.sort();

                assert_eq!(addresses.len(), 12);
                assert_eq!(addresses, cluster_addresses);
                assert_eq!(infos.len(), 12);
                for i in 0..12 {
                    let split: Vec<_> = addresses[i].split(':').collect();
                    assert!(infos[i].contains(&format!("tcp_port:{}", split[1])));
                }

                let route_to_all_primaries =
                    redis::cluster_routing::MultipleNodeRoutingInfo::AllMasters;
                let routing = RoutingInfo::MultiNode((route_to_all_primaries, None));
                let res = connection
                    .route_command(&redis::cmd("INFO"), routing)
                    .await
                    .unwrap();
                let (addresses, infos) = split_to_addresses_and_info(res);
                assert_eq!(addresses.len(), 6);
                assert_eq!(infos.len(), 6);
                // verify that all primaries have the correct port & host, and are marked as primaries.
                for i in 0..6 {
                    assert!(cluster_addresses.contains(&addresses[i]));
                    let split: Vec<_> = addresses[i].split(':').collect();
                    assert!(infos[i].contains(&format!("tcp_port:{}", split[1])));
                    assert!(infos[i].contains("role:primary") || infos[i].contains("role:master"));
                }

                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_cluster_resp3(#[case] runtime: RuntimeType) {
        if use_protocol() == ProtocolVersion::RESP2 {
            return;
        }
        block_on_all(
            async move {
                let cluster = TestClusterContext::new();

                let mut connection = cluster.async_connection().await;

                let _: () = connection.hset("hash", "foo", "baz").await.unwrap();
                let _: () = connection.hset("hash", "bar", "foobar").await.unwrap();
                let result: Value = connection.hgetall("hash").await.unwrap();

                assert_eq!(
                    result,
                    Value::Map(vec![
                        (
                            Value::BulkString("foo".as_bytes().to_vec()),
                            Value::BulkString("baz".as_bytes().to_vec())
                        ),
                        (
                            Value::BulkString("bar".as_bytes().to_vec()),
                            Value::BulkString("foobar".as_bytes().to_vec())
                        )
                    ])
                );

                Ok(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_basic_pipe(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new();

        block_on_all(
            async move {
                let mut connection = cluster.async_connection().await;
                let mut pipe = redis::pipe();
                pipe.add_command(cmd("SET").arg("test").arg("test_data").clone());
                pipe.add_command(cmd("SET").arg("{test}3").arg("test_data3").clone());
                pipe.exec_async(&mut connection).await?;
                let res: String = connection.get("test").await?;
                assert_eq!(res, "test_data");
                let res: String = connection.get("{test}3").await?;
                assert_eq!(res, "test_data3");
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap()
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_multi_shard_commands(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new();

        block_on_all(
            async move {
                let mut connection = cluster.async_connection().await;

                let res: String = connection
                    .mset(&[("foo", "bar"), ("bar", "foo"), ("baz", "bazz")])
                    .await?;
                assert_eq!(res, "OK");
                let res: Vec<String> = connection.mget(&["baz", "foo", "bar"]).await?;
                assert_eq!(res, vec!["bazz", "bar", "foo"]);
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap()
    }

    #[cfg(feature = "tls-rustls")]
    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    fn test_async_cluster_default_reject_invalid_hostnames(#[case] runtime: RuntimeType) {
        use redis_test::cluster::ClusterType;

        if ClusterType::get_intended() != ClusterType::TcpTls {
            // Only TLS causes invalid certificates to be rejected as desired.
            return;
        }

        let cluster = TestClusterContext::new_with_config(RedisClusterConfiguration {
            tls_insecure: false,
            certs_with_ip_alts: false,
            ..Default::default()
        });

        block_on_all(
            async move {
                assert!(cluster.client.get_async_connection().await.is_err());
                Ok(())
            },
            runtime,
        )
        .unwrap();
    }

    #[cfg(feature = "tls-rustls-insecure")]
    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    fn test_async_cluster_danger_accept_invalid_hostnames(#[case] runtime: RuntimeType) {
        use redis_test::cluster::ClusterType;

        if ClusterType::get_intended() != ClusterType::TcpTls {
            // No point testing this TLS-specific mode in non-TLS configurations.
            return;
        }

        let cluster = TestClusterContext::new_with_config_and_builder(
            RedisClusterConfiguration {
                tls_insecure: false,
                certs_with_ip_alts: false,
                ..Default::default()
            },
            |builder| builder.danger_accept_invalid_hostnames(true),
        );

        block_on_all(
            async move {
                let connection = cluster.async_connection().await;
                smoke_test_connection(connection).await;
                Ok(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_basic_failover(#[case] runtime: RuntimeType) {
        block_on_all(
            async move {
                test_failover(
                    &TestClusterContext::new_with_config(
                        RedisClusterConfiguration::single_replica_config(),
                    ),
                    10,
                    123,
                    false,
                )
                .await;
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap()
    }

    async fn do_failover(
        redis: &mut redis::aio::MultiplexedConnection,
    ) -> Result<(), anyhow::Error> {
        cmd("CLUSTER").arg("FAILOVER").exec_async(redis).await?;
        Ok(())
    }

    // parameter `_mtls_enabled` can only be used if `feature = tls-rustls` is active
    #[allow(dead_code)]
    async fn test_failover(
        env: &TestClusterContext,
        requests: i32,
        value: i32,
        _mtls_enabled: bool,
    ) {
        let completed = Arc::new(AtomicI32::new(0));

        let connection = env.async_connection().await;
        let mut node_conns: Vec<MultiplexedConnection> = Vec::new();

        'outer: loop {
            node_conns.clear();
            let cleared_nodes = async {
                for server in env.cluster.iter_servers() {
                    let addr = server.client_addr();

                    let client = build_single_client(
                        server.connection_info(),
                        &server.tls_paths,
                        _mtls_enabled,
                    )
                    .unwrap_or_else(|e| panic!("Failed to connect to '{addr}': {e}"));

                    let mut conn = client
                        .get_multiplexed_async_connection()
                        .await
                        .unwrap_or_else(|e| panic!("Failed to get connection: {e}"));

                    let info: InfoDict = redis::Cmd::new()
                        .arg("INFO")
                        .query_async(&mut conn)
                        .await
                        .expect("INFO");
                    let role: String = info.get("role").expect("cluster role");

                    if role == "master" {
                        async { Ok(conn.flushall::<()>().await?) }
                            .timeout(futures_time::time::Duration::from_secs(3))
                            .await
                            .unwrap_or_else(|err| Err(anyhow::Error::from(err)))?;
                    }

                    node_conns.push(conn);
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            match cleared_nodes {
                Ok(()) => break 'outer,
                Err(err) => {
                    // Failed to clear the databases, retry
                    log::warn!("{err}");
                }
            }
        }

        let _: () = (0..requests + 1)
            .map(|i| {
                let mut connection = connection.clone();
                let mut node_conns = node_conns.clone();
                let completed = completed.clone();
                async move {
                    if i == requests / 2 {
                        // Failover all the nodes, error only if all the failover requests error
                        let mut results = future::join_all(
                            node_conns
                                .iter_mut()
                                .map(|conn| Box::pin(do_failover(conn))),
                        )
                        .await;
                        if results.iter().all(|res| res.is_err()) {
                            results.pop().unwrap()
                        } else {
                            Ok::<_, anyhow::Error>(())
                        }
                    } else {
                        let key = format!("test-{value}-{i}");
                        cmd("SET")
                            .arg(&key)
                            .arg(i)
                            .clone()
                            .exec_async(&mut connection)
                            .await?;
                        let res: i32 = cmd("GET")
                            .arg(key)
                            .clone()
                            .query_async(&mut connection)
                            .await?;
                        assert_eq!(res, i);
                        completed.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, anyhow::Error>(())
                    }
                }
            })
            .collect::<stream::FuturesUnordered<_>>()
            .try_collect()
            .await
            .unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(
            completed.load(Ordering::SeqCst),
            requests,
            "Some requests never completed!"
        );
    }

    static ERROR: Lazy<AtomicBool> = Lazy::new(Default::default);

    #[derive(Clone)]
    struct ErrorConnection {
        inner: MultiplexedConnection,
    }

    impl Connect for ErrorConnection {
        fn connect_with_config<'a, T>(
            info: T,
            config: redis::AsyncConnectionConfig,
        ) -> RedisFuture<'a, Self>
        where
            T: IntoConnectionInfo + Send + 'a,
        {
            Box::pin(async move {
                let inner = MultiplexedConnection::connect_with_config(info, config).await?;
                Ok(ErrorConnection { inner })
            })
        }
    }

    impl ConnectionLike for ErrorConnection {
        fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
            if ERROR.load(Ordering::SeqCst) {
                Box::pin(async move { Err(RedisError::from((redis::ErrorKind::Moved, "ERROR"))) })
            } else {
                self.inner.req_packed_command(cmd)
            }
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            pipeline: &'a redis::Pipeline,
            offset: usize,
            count: usize,
        ) -> RedisFuture<'a, Vec<Value>> {
            self.inner.req_packed_commands(pipeline, offset, count)
        }

        fn get_db(&self) -> i64 {
            self.inner.get_db()
        }
    }

    #[rstest]
    fn test_async_cluster_error_in_inner_connection() {
        let cluster = TestClusterContext::new();

        block_on_all(
            async move {
                let mut con = cluster.async_generic_connection::<ErrorConnection>().await;

                ERROR.store(false, Ordering::SeqCst);
                let r: Option<i32> = con.get("test").await?;
                assert_eq!(r, None::<i32>);

                ERROR.store(true, Ordering::SeqCst);

                let result: RedisResult<()> = con.get("test").await;
                assert_eq!(
                    result,
                    Err(RedisError::from((redis::ErrorKind::Moved, "ERROR")))
                );

                Ok::<_, RedisError>(())
            },
            RuntimeType::Tokio,
        )
        .unwrap();
    }

    #[test]
    fn test_cluster_async_can_connect_to_server_that_sends_cluster_slots_with_null_host_name() {
        let name =
            "test_cluster_async_can_connect_to_server_that_sends_cluster_slots_with_null_host_name";

        let MockEnv {
            runtime,
            async_connection: mut connection,
            ..
        } = MockEnv::new(name, move |cmd: &[u8], _| {
            if contains_slice(cmd, b"PING") {
                Err(Ok(Value::SimpleString("OK".into())))
            } else if contains_slice(cmd, b"CLUSTER") && contains_slice(cmd, b"SLOTS") {
                Err(Ok(Value::Array(vec![Value::Array(vec![
                    Value::Int(0),
                    Value::Int(16383),
                    Value::Array(vec![Value::Nil, Value::Int(6379)]),
                ])])))
            } else {
                Err(Ok(Value::Nil))
            }
        });

        let value = runtime.block_on(cmd("GET").arg("test").query_async::<Value>(&mut connection));

        assert_eq!(value, Ok(Value::Nil));
    }

    #[test]
    fn test_cluster_async_can_connect_to_server_that_sends_cluster_slots_with_partial_nodes_with_unknown_host_name(
    ) {
        let name = "test_cluster_async_can_connect_to_server_that_sends_cluster_slots_with_partial_nodes_with_unknown_host_name";

        let MockEnv {
            runtime,
            async_connection: mut connection,
            ..
        } = MockEnv::new(name, move |cmd: &[u8], _| {
            if contains_slice(cmd, b"PING") {
                Err(Ok(Value::SimpleString("OK".into())))
            } else if contains_slice(cmd, b"CLUSTER") && contains_slice(cmd, b"SLOTS") {
                Err(Ok(Value::Array(vec![
                    Value::Array(vec![
                        Value::Int(0),
                        Value::Int(7000),
                        Value::Array(vec![
                            Value::BulkString(name.as_bytes().to_vec()),
                            Value::Int(6379),
                        ]),
                    ]),
                    Value::Array(vec![
                        Value::Int(7001),
                        Value::Int(16383),
                        Value::Array(vec![
                            Value::BulkString("?".as_bytes().to_vec()),
                            Value::Int(6380),
                        ]),
                    ]),
                ])))
            } else {
                Err(Ok(Value::Nil))
            }
        });

        let value = runtime.block_on(cmd("GET").arg("test").query_async::<Value>(&mut connection));

        assert_eq!(value, Ok(Value::Nil));
    }

    #[test]
    fn test_async_cluster_retries() {
        let name = "tryagain";

        let requests = atomic::AtomicUsize::new(0);
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]).retries(5),
            name,
            move |cmd: &[u8], _| {
                respond_startup(name, cmd)?;

                match requests.fetch_add(1, atomic::Ordering::SeqCst) {
                    0..=4 => Err(parse_redis_value(b"-TRYAGAIN mock\r\n")),
                    _ => Err(Ok(Value::BulkString(b"123".to_vec()))),
                }
            },
        );

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        assert_eq!(value, Ok(Some(123)));
    }

    #[test]
    fn test_async_cluster_tryagain_exhaust_retries() {
        let name = "tryagain_exhaust_retries";

        let requests = Arc::new(atomic::AtomicUsize::new(0));

        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]).retries(2),
            name,
            {
                let requests = requests.clone();
                move |cmd: &[u8], _| {
                    respond_startup(name, cmd)?;
                    requests.fetch_add(1, atomic::Ordering::SeqCst);
                    Err(parse_redis_value(b"-TRYAGAIN mock\r\n"))
                }
            },
        );

        let result = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        match result {
            Ok(_) => panic!("result should be an error"),
            Err(e) => match e.kind() {
                ErrorKind::TryAgain => {}
                _ => panic!("Expected TryAgain but got {:?}", e.kind()),
            },
        }
        assert_eq!(requests.load(atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn test_async_cluster_move_error_when_new_node_is_added() {
        let name = "rebuild_with_extra_nodes";

        let requests = atomic::AtomicUsize::new(0);
        let started = atomic::AtomicBool::new(false);
        let refreshed = atomic::AtomicBool::new(false);

        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::new(name, move |cmd: &[u8], port| {
            if !started.load(atomic::Ordering::SeqCst) {
                respond_startup(name, cmd)?;
            }
            started.store(true, atomic::Ordering::SeqCst);

            if contains_slice(cmd, b"PING") {
                return Err(Ok(Value::SimpleString("OK".into())));
            }

            let i = requests.fetch_add(1, atomic::Ordering::SeqCst);

            let is_get_cmd = contains_slice(cmd, b"GET");
            let get_response = Err(Ok(Value::BulkString(b"123".to_vec())));
            match i {
                // Respond that the key exists on a node that does not yet have a connection:
                0 => Err(parse_redis_value(
                    format!("-MOVED 123 {name}:6380\r\n").as_bytes(),
                )),
                _ => {
                    if contains_slice(cmd, b"CLUSTER") && contains_slice(cmd, b"SLOTS") {
                        // Should not attempt to refresh slots more than once:
                        assert!(!refreshed.swap(true, Ordering::SeqCst));
                        Err(Ok(Value::Array(vec![
                            Value::Array(vec![
                                Value::Int(0),
                                Value::Int(1),
                                Value::Array(vec![
                                    Value::BulkString(name.as_bytes().to_vec()),
                                    Value::Int(6379),
                                ]),
                            ]),
                            Value::Array(vec![
                                Value::Int(2),
                                Value::Int(16383),
                                Value::Array(vec![
                                    Value::BulkString(name.as_bytes().to_vec()),
                                    Value::Int(6380),
                                ]),
                            ]),
                        ])))
                    } else {
                        assert_eq!(port, 6380);
                        assert!(is_get_cmd, "{:?}", std::str::from_utf8(cmd));
                        get_response
                    }
                }
            }
        });

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        assert_eq!(value, Ok(Some(123)));
    }

    #[test]
    fn test_async_cluster_refresh_topology_even_with_zero_retries() {
        let name = "test_async_cluster_refresh_topology_even_with_zero_retries";

        let should_refresh = atomic::AtomicBool::new(false);

        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]).retries(0),
            name,
            move |cmd: &[u8], port| {
                if !should_refresh.load(atomic::Ordering::SeqCst) {
                    respond_startup(name, cmd)?;
                }

                if contains_slice(cmd, b"PING") {
                    return Err(Ok(Value::SimpleString("OK".into())));
                }

                if contains_slice(cmd, b"CLUSTER") && contains_slice(cmd, b"SLOTS") {
                    return Err(Ok(Value::Array(vec![
                        Value::Array(vec![
                            Value::Int(0),
                            Value::Int(1),
                            Value::Array(vec![
                                Value::BulkString(name.as_bytes().to_vec()),
                                Value::Int(6379),
                            ]),
                        ]),
                        Value::Array(vec![
                            Value::Int(2),
                            Value::Int(16383),
                            Value::Array(vec![
                                Value::BulkString(name.as_bytes().to_vec()),
                                Value::Int(6380),
                            ]),
                        ]),
                    ])));
                }

                if contains_slice(cmd, b"GET") {
                    let get_response = Err(Ok(Value::BulkString(b"123".to_vec())));
                    match port {
                        6380 => get_response,
                        // Respond that the key exists on a node that does not yet have a connection:
                        _ => {
                            // Should not attempt to refresh slots more than once:
                            assert!(!should_refresh.swap(true, Ordering::SeqCst));
                            Err(parse_redis_value(
                                format!("-MOVED 123 {name}:6380\r\n").as_bytes(),
                            ))
                        }
                    }
                } else {
                    panic!("unexpected command {cmd:?}")
                }
            },
        );

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        // The user should receive an initial error, because there are no retries and the first request failed.
        assert_eq!(
            value,
            Err(RedisError::from((
                ErrorKind::Moved,
                "An error was signalled by the server",
                "test_async_cluster_refresh_topology_even_with_zero_retries:6380".to_string()
            )))
        );

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        assert_eq!(value, Ok(Some(123)));
    }

    #[test]
    fn test_async_cluster_reconnect_even_with_zero_retries() {
        let name = "test_async_cluster_reconnect_even_with_zero_retries";

        let should_reconnect = atomic::AtomicBool::new(true);
        let connection_count = Arc::new(atomic::AtomicU16::new(0));
        let connection_count_clone = connection_count.clone();

        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]).retries(0),
            name,
            move |cmd: &[u8], port| {
                match respond_startup(name, cmd) {
                    Ok(_) => {}
                    Err(err) => {
                        connection_count.fetch_add(1, Ordering::Relaxed);
                        return Err(err);
                    }
                }

                if contains_slice(cmd, b"ECHO") && port == 6379 {
                    // Should not attempt to refresh slots more than once:
                    if should_reconnect.swap(false, Ordering::SeqCst) {
                        Err(Err(broken_pipe_error()))
                    } else {
                        Err(Ok(Value::BulkString(b"PONG".to_vec())))
                    }
                } else {
                    panic!("unexpected command {cmd:?}")
                }
            },
        );

        // 4 - MockEnv creates a sync & async connections, each calling CLUSTER SLOTS once & PING per node.
        // If we add more nodes or more setup calls, this number should increase.
        assert_eq!(connection_count_clone.load(Ordering::Relaxed), 4);

        let value = runtime.block_on(connection.route_command(
            &cmd("ECHO"),
            RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
                host: name.to_string(),
                port: 6379,
            }),
        ));

        // The user should receive an initial error, because there are no retries and the first request failed.
        assert_eq!(
            value.unwrap_err().to_string(),
            broken_pipe_error().to_string()
        );

        let value = runtime.block_on(connection.route_command(
            &cmd("ECHO"),
            RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
                host: name.to_string(),
                port: 6379,
            }),
        ));

        assert_eq!(value, Ok(Value::BulkString(b"PONG".to_vec())));
        // 5 - because of the 4 above, and then another PING for new connections.
        assert_eq!(connection_count_clone.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn test_async_cluster_ask_redirect() {
        let name = "test_async_cluster_ask_redirect";
        let completed = Arc::new(AtomicI32::new(0));
        let MockEnv {
            async_connection: mut connection,
            handler: _handler,
            runtime,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]),
            name,
            {
                move |cmd: &[u8], port| {
                    respond_startup_two_nodes(name, cmd)?;
                    // Error twice with io-error, ensure connection is reestablished w/out calling
                    // other node (i.e., not doing a full slot rebuild)
                    let count = completed.fetch_add(1, Ordering::SeqCst);
                    match port {
                        6379 => match count {
                            0 => Err(parse_redis_value(
                                b"-ASK 14000 test_async_cluster_ask_redirect:6380\r\n",
                            )),
                            _ => panic!("Node should not be called now"),
                        },
                        6380 => match count {
                            1 => {
                                assert!(contains_slice(cmd, b"ASKING"));
                                Err(Ok(Value::Okay))
                            }
                            2 => {
                                assert!(contains_slice(cmd, b"GET"));
                                Err(Ok(Value::BulkString(b"123".to_vec())))
                            }
                            _ => panic!("Node should not be called now"),
                        },
                        _ => panic!("Wrong node"),
                    }
                }
            },
        );

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        assert_eq!(value, Ok(Some(123)));
    }

    #[test]
    fn test_async_cluster_ask_save_new_connection() {
        let name = "test_async_cluster_ask_save_new_connection";
        let ping_attempts = Arc::new(AtomicI32::new(0));
        let ping_attempts_clone = ping_attempts.clone();
        let MockEnv {
            async_connection: mut connection,
            handler: _handler,
            runtime,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]),
            name,
            {
                move |cmd: &[u8], port| {
                    if port != 6391 {
                        respond_startup_two_nodes(name, cmd)?;
                        return Err(parse_redis_value(
                            b"-ASK 14000 test_async_cluster_ask_save_new_connection:6391\r\n",
                        ));
                    }

                    if contains_slice(cmd, b"PING") {
                        ping_attempts_clone.fetch_add(1, Ordering::Relaxed);
                    }
                    respond_startup_two_nodes(name, cmd)?;
                    Err(Ok(Value::Okay))
                }
            },
        );

        for _ in 0..4 {
            runtime
                .block_on(cmd("GET").arg("test").query_async::<Value>(&mut connection))
                .unwrap();
        }

        assert_eq!(ping_attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_async_cluster_reset_routing_if_redirect_fails() {
        let name = "test_async_cluster_reset_routing_if_redirect_fails";
        let completed = Arc::new(AtomicI32::new(0));
        let MockEnv {
            async_connection: mut connection,
            handler: _handler,
            runtime,
            ..
        } = MockEnv::new(name, move |cmd: &[u8], port| {
            if port != 6379 && port != 6380 {
                return Err(Err(broken_pipe_error()));
            }
            respond_startup_two_nodes(name, cmd)?;
            let count = completed.fetch_add(1, Ordering::SeqCst);
            match (port, count) {
                // redirect once to non-existing node
                (6379, 0) => Err(parse_redis_value(
                    format!("-ASK 14000 {name}:9999\r\n").as_bytes(),
                )),
                // accept the next request
                (6379, 1) => {
                    assert!(contains_slice(cmd, b"GET"));
                    Err(Ok(Value::BulkString(b"123".to_vec())))
                }
                _ => panic!("Wrong node. port: {port}, received count: {count}"),
            }
        });

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        assert_eq!(value, Ok(Some(123)));
    }

    #[test]
    fn test_async_cluster_ask_redirect_even_if_original_call_had_no_route() {
        let name = "test_async_cluster_ask_redirect_even_if_original_call_had_no_route";
        let completed = Arc::new(AtomicI32::new(0));
        let MockEnv {
            async_connection: mut connection,
            handler: _handler,
            runtime,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]),
            name,
            {
                move |cmd: &[u8], port| {
                    respond_startup_two_nodes(name, cmd)?;
                    // Error twice with io-error, ensure connection is reestablished w/out calling
                    // other node (i.e., not doing a full slot rebuild)
                    let count = completed.fetch_add(1, Ordering::SeqCst);
                    if count == 0 {
                        return Err(parse_redis_value(b"-ASK 14000 test_async_cluster_ask_redirect_even_if_original_call_had_no_route:6380\r\n"));
                    }
                    match port {
                        6380 => match count {
                            1 => {
                                assert!(
                                    contains_slice(cmd, b"ASKING"),
                                    "{:?}",
                                    std::str::from_utf8(cmd)
                                );
                                Err(Ok(Value::Okay))
                            }
                            2 => {
                                assert!(contains_slice(cmd, b"EVAL"));
                                Err(Ok(Value::Okay))
                            }
                            _ => panic!("Node should not be called now"),
                        },
                        _ => panic!("Wrong node"),
                    }
                }
            },
        );

        let value = runtime.block_on(
            cmd("EVAL") // Eval command has no directed, and so is redirected randomly
                .query_async::<Value>(&mut connection),
        );

        assert_eq!(value, Ok(Value::Okay));
    }

    #[test]
    fn test_async_cluster_ask_error_when_new_node_is_added() {
        let name = "ask_with_extra_nodes";

        let requests = atomic::AtomicUsize::new(0);
        let started = atomic::AtomicBool::new(false);

        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::new(name, move |cmd: &[u8], port| {
            if !started.load(atomic::Ordering::SeqCst) {
                respond_startup(name, cmd)?;
            }
            started.store(true, atomic::Ordering::SeqCst);

            if contains_slice(cmd, b"PING") {
                return Err(Ok(Value::SimpleString("OK".into())));
            }

            let i = requests.fetch_add(1, atomic::Ordering::SeqCst);

            match i {
                // Respond that the key exists on a node that does not yet have a connection:
                0 => Err(parse_redis_value(
                    format!("-ASK 123 {name}:6380\r\n").as_bytes(),
                )),
                1 => {
                    assert_eq!(port, 6380);
                    assert!(contains_slice(cmd, b"ASKING"));
                    Err(Ok(Value::Okay))
                }
                2 => {
                    assert_eq!(port, 6380);
                    assert!(contains_slice(cmd, b"GET"));
                    Err(Ok(Value::BulkString(b"123".to_vec())))
                }
                _ => {
                    panic!("Unexpected request: {cmd:?}");
                }
            }
        });

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        assert_eq!(value, Ok(Some(123)));
    }

    #[test]
    fn test_async_cluster_replica_read() {
        let name = "test_async_cluster_replica_read";

        // requests should route to replica
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |cmd: &[u8], port| {
                respond_startup_with_replica(name, cmd)?;
                match port {
                    6380 => Err(Ok(Value::BulkString(b"123".to_vec()))),
                    _ => panic!("Wrong node"),
                }
            },
        );

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );
        assert_eq!(value, Ok(Some(123)));

        // requests should route to primary
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |cmd: &[u8], port| {
                respond_startup_with_replica(name, cmd)?;
                match port {
                    6379 => Err(Ok(Value::SimpleString("OK".into()))),
                    _ => panic!("Wrong node"),
                }
            },
        );

        let value = runtime.block_on(
            cmd("SET")
                .arg("test")
                .arg("123")
                .query_async::<Option<Value>>(&mut connection),
        );
        assert_eq!(value, Ok(Some(Value::SimpleString("OK".to_owned()))));
    }

    fn test_async_cluster_fan_out(
        name: &'static str,
        command: &'static str,
        expected_ports: Vec<u16>,
        slots_config: Option<Vec<MockSlotRange>>,
    ) {
        let found_ports = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ports_clone = found_ports.clone();
        let mut cmd = Cmd::new();
        for arg in command.split_whitespace() {
            cmd.arg(arg);
        }
        let packed_cmd = cmd.get_packed_command();
        // requests should route to replica
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(
                    name,
                    received_cmd,
                    slots_config.clone(),
                )?;
                if received_cmd == packed_cmd {
                    ports_clone.lock().unwrap().push(port);
                    return Err(Ok(Value::SimpleString("OK".into())));
                }
                Ok(())
            },
        );

        let _ = runtime.block_on(cmd.query_async::<Option<()>>(&mut connection));
        found_ports.lock().unwrap().sort();
        // MockEnv creates 2 mock connections.
        assert_eq!(*found_ports.lock().unwrap(), expected_ports);
    }

    #[test]
    fn test_async_cluster_fan_out_to_all_primaries() {
        test_async_cluster_fan_out(
            "test_async_cluster_fan_out_to_all_primaries",
            "FLUSHALL",
            vec![6379, 6381],
            None,
        );
    }

    #[test]
    fn test_async_cluster_fan_out_to_all_nodes() {
        test_async_cluster_fan_out(
            "test_async_cluster_fan_out_to_all_nodes",
            "CONFIG SET",
            vec![6379, 6380, 6381, 6382],
            None,
        );
    }

    #[test]
    fn test_async_cluster_fan_out_once_to_each_primary_when_no_replicas_are_available() {
        test_async_cluster_fan_out(
            "test_async_cluster_fan_out_once_to_each_primary_when_no_replicas_are_available",
            "CONFIG SET",
            vec![6379, 6381],
            Some(vec![
                MockSlotRange {
                    primary_port: 6379,
                    replica_ports: Vec::new(),
                    slot_range: (0..8191),
                },
                MockSlotRange {
                    primary_port: 6381,
                    replica_ports: Vec::new(),
                    slot_range: (8192..16383),
                },
            ]),
        );
    }

    #[test]
    fn test_async_cluster_fan_out_once_even_if_primary_has_multiple_slot_ranges() {
        test_async_cluster_fan_out(
            "test_async_cluster_fan_out_once_even_if_primary_has_multiple_slot_ranges",
            "CONFIG SET",
            vec![6379, 6380, 6381, 6382],
            Some(vec![
                MockSlotRange {
                    primary_port: 6379,
                    replica_ports: vec![6380],
                    slot_range: (0..4000),
                },
                MockSlotRange {
                    primary_port: 6381,
                    replica_ports: vec![6382],
                    slot_range: (4001..8191),
                },
                MockSlotRange {
                    primary_port: 6379,
                    replica_ports: vec![6380],
                    slot_range: (8192..8200),
                },
                MockSlotRange {
                    primary_port: 6381,
                    replica_ports: vec![6382],
                    slot_range: (8201..16383),
                },
            ]),
        );
    }

    #[test]
    fn test_async_cluster_route_according_to_passed_argument() {
        let name = "test_async_cluster_route_according_to_passed_argument";

        let touched_ports = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cloned_ports = touched_ports.clone();

        // requests should route to replica
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |cmd: &[u8], port| {
                respond_startup_with_replica(name, cmd)?;
                cloned_ports.lock().unwrap().push(port);
                Err(Ok(Value::Nil))
            },
        );

        let mut cmd = cmd("GET");
        cmd.arg("test");
        let _ = runtime.block_on(connection.route_command(
            &cmd,
            RoutingInfo::MultiNode((MultipleNodeRoutingInfo::AllMasters, None)),
        ));
        {
            let mut touched_ports = touched_ports.lock().unwrap();
            touched_ports.sort();
            assert_eq!(*touched_ports, vec![6379, 6381]);
            touched_ports.clear();
        }

        let _ = runtime.block_on(connection.route_command(
            &cmd,
            RoutingInfo::MultiNode((MultipleNodeRoutingInfo::AllNodes, None)),
        ));
        {
            let mut touched_ports = touched_ports.lock().unwrap();
            touched_ports.sort();
            assert_eq!(*touched_ports, vec![6379, 6380, 6381, 6382]);
            touched_ports.clear();
        }

        let _ = runtime.block_on(connection.route_command(
            &cmd,
            RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
                host: name.to_string(),
                port: 6382,
            }),
        ));
        {
            let mut touched_ports = touched_ports.lock().unwrap();
            touched_ports.sort();
            assert_eq!(*touched_ports, vec![6382]);
            touched_ports.clear();
        }
    }

    #[test]
    fn test_async_cluster_fan_out_and_aggregate_numeric_response_with_min() {
        let name = "test_async_cluster_fan_out_and_aggregate_numeric_response";
        let mut cmd = Cmd::new();
        cmd.arg("SLOWLOG").arg("LEN");

        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;

                let res = 6383 - port as i64;
                Err(Ok(Value::Int(res))) // this results in 1,2,3,4
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<i64>(&mut connection))
            .unwrap();
        assert_eq!(result, 10, "{result}");
    }

    #[test]
    fn test_async_cluster_fan_out_and_aggregate_logical_array_response() {
        let name = "test_async_cluster_fan_out_and_aggregate_logical_array_response";
        let mut cmd = Cmd::new();
        cmd.arg("SCRIPT")
            .arg("EXISTS")
            .arg("foo")
            .arg("bar")
            .arg("baz")
            .arg("barvaz");

        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;

                if port == 6381 {
                    return Err(Ok(Value::Array(vec![
                        Value::Int(0),
                        Value::Int(0),
                        Value::Int(1),
                        Value::Int(1),
                    ])));
                } else if port == 6379 {
                    return Err(Ok(Value::Array(vec![
                        Value::Int(0),
                        Value::Int(1),
                        Value::Int(0),
                        Value::Int(1),
                    ])));
                }

                panic!("unexpected port {port}");
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<Vec<i64>>(&mut connection))
            .unwrap();
        assert_eq!(result, vec![0, 0, 0, 1], "{result:?}");
    }

    #[test]
    fn test_async_cluster_fan_out_and_return_one_succeeded_response() {
        let name = "test_async_cluster_fan_out_and_return_one_succeeded_response";
        let mut cmd = Cmd::new();
        cmd.arg("SCRIPT").arg("KILL");
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;
                if port == 6381 {
                    return Err(Ok(Value::Okay));
                } else if port == 6379 {
                    return Err(Err((
                        ErrorKind::NotBusy,
                        "No scripts in execution right now",
                    )
                        .into()));
                }

                panic!("unexpected port {port}");
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<Value>(&mut connection))
            .unwrap();
        assert_eq!(result, Value::Okay, "{result:?}");
    }

    #[test]
    fn test_async_cluster_fan_out_and_fail_one_succeeded_if_there_are_no_successes() {
        let name = "test_async_cluster_fan_out_and_fail_one_succeeded_if_there_are_no_successes";
        let mut cmd = Cmd::new();
        cmd.arg("SCRIPT").arg("KILL");
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], _port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;

                Err(Err((
                    ErrorKind::NotBusy,
                    "No scripts in execution right now",
                )
                    .into()))
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<Value>(&mut connection))
            .unwrap_err();
        assert_eq!(result.kind(), ErrorKind::NotBusy, "{:?}", result.kind());
    }

    #[test]
    fn test_async_cluster_fan_out_and_return_all_succeeded_response() {
        let name = "test_async_cluster_fan_out_and_return_all_succeeded_response";
        let cmd = cmd("FLUSHALL");
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], _port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;
                Err(Ok(Value::Okay))
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<Value>(&mut connection))
            .unwrap();
        assert_eq!(result, Value::Okay, "{result:?}");
    }

    #[test]
    fn test_async_cluster_fan_out_and_fail_all_succeeded_if_there_is_a_single_failure() {
        let name = "test_async_cluster_fan_out_and_fail_all_succeeded_if_there_is_a_single_failure";
        let cmd = cmd("FLUSHALL");
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;
                if port == 6381 {
                    return Err(Err((
                        ErrorKind::NotBusy,
                        "No scripts in execution right now",
                    )
                        .into()));
                }
                Err(Ok(Value::Okay))
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<Value>(&mut connection))
            .unwrap_err();
        assert_eq!(result.kind(), ErrorKind::NotBusy, "{:?}", result.kind());
    }

    #[test]
    fn test_async_cluster_fan_out_and_return_one_succeeded_ignoring_empty_values() {
        let name = "test_async_cluster_fan_out_and_return_one_succeeded_ignoring_empty_values";
        let cmd = cmd("RANDOMKEY");
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;
                if port == 6381 {
                    return Err(Ok(Value::BulkString("foo".as_bytes().to_vec())));
                }
                Err(Ok(Value::Nil))
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<String>(&mut connection))
            .unwrap();
        assert_eq!(result, "foo", "{result:?}");
    }

    #[test]
    fn test_async_cluster_fan_out_and_return_map_of_results_for_special_response_policy() {
        let name = "foo";
        let mut cmd = Cmd::new();
        cmd.arg("LATENCY").arg("LATEST");
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;
                Err(Ok(Value::BulkString(
                    format!("latency: {port}").into_bytes(),
                )))
            },
        );

        // TODO once RESP3 is in, return this as a map
        let mut result = runtime
            .block_on(cmd.query_async::<Vec<(String, String)>>(&mut connection))
            .unwrap();
        result.sort();
        assert_eq!(
            result,
            vec![
                (format!("{name}:6379"), "latency: 6379".to_string()),
                (format!("{name}:6380"), "latency: 6380".to_string()),
                (format!("{name}:6381"), "latency: 6381".to_string()),
                (format!("{name}:6382"), "latency: 6382".to_string())
            ],
            "{result:?}"
        );
    }

    #[test]
    fn test_async_cluster_fan_out_and_combine_arrays_of_values() {
        let name = "foo";
        let cmd = cmd("KEYS");
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;
                Err(Ok(Value::Array(vec![Value::BulkString(
                    format!("key:{port}").into_bytes(),
                )])))
            },
        );

        let mut result = runtime
            .block_on(cmd.query_async::<Vec<String>>(&mut connection))
            .unwrap();
        result.sort();
        assert_eq!(
            result,
            vec!["key:6379".to_string(), "key:6381".to_string(),],
            "{result:?}"
        );
    }

    #[test]
    fn test_async_cluster_split_multi_shard_command_and_combine_arrays_of_values() {
        let name = "test_async_cluster_split_multi_shard_command_and_combine_arrays_of_values";
        let mut cmd = cmd("MGET");
        cmd.arg("foo").arg("bar").arg("baz");
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;
                let cmd_str = std::str::from_utf8(received_cmd).unwrap();
                let results = ["foo", "bar", "baz"]
                    .iter()
                    .filter_map(|expected_key| {
                        if cmd_str.contains(expected_key) {
                            Some(Value::BulkString(
                                format!("{expected_key}-{port}").into_bytes(),
                            ))
                        } else {
                            None
                        }
                    })
                    .collect();
                Err(Ok(Value::Array(results)))
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<Vec<String>>(&mut connection))
            .unwrap();
        assert_eq!(result, vec!["foo-6382", "bar-6380", "baz-6380"]);
    }

    #[test]
    fn test_async_cluster_handle_asking_error_in_split_multi_shard_command() {
        let name = "test_async_cluster_handle_asking_error_in_split_multi_shard_command";
        let mut cmd = cmd("MGET");
        cmd.arg("foo").arg("bar").arg("baz");
        let asking_called = Arc::new(AtomicU16::new(0));
        let asking_called_cloned = asking_called.clone();
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]).read_from_replicas(),
            name,
            move |received_cmd: &[u8], port| {
                respond_startup_with_replica_using_config(name, received_cmd, None)?;
                let cmd_str = std::str::from_utf8(received_cmd).unwrap();
                if cmd_str.contains("ASKING") && port == 6382 {
                    asking_called_cloned.fetch_add(1, Ordering::Relaxed);
                }
                if port == 6380 && cmd_str.contains("baz") {
                    return Err(parse_redis_value(
                        format!("-ASK 14000 {name}:6382\r\n").as_bytes(),
                    ));
                }
                let results = ["foo", "bar", "baz"]
                    .iter()
                    .filter_map(|expected_key| {
                        if cmd_str.contains(expected_key) {
                            Some(Value::BulkString(
                                format!("{expected_key}-{port}").into_bytes(),
                            ))
                        } else {
                            None
                        }
                    })
                    .collect();
                Err(Ok(Value::Array(results)))
            },
        );

        let result = runtime
            .block_on(cmd.query_async::<Vec<String>>(&mut connection))
            .unwrap();
        assert_eq!(result, vec!["foo-6382", "bar-6380", "baz-6382"]);
        assert_eq!(asking_called.load(Ordering::Relaxed), 1);
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_with_username_and_password(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new_insecure_with_cluster_client_builder(|builder| {
            builder
                .username(RedisCluster::username().to_string())
                .password(RedisCluster::password().to_string())
        });
        cluster.disable_default_user();

        block_on_all(
            async move {
                let mut connection = cluster.async_connection().await;
                cmd("SET")
                    .arg("test")
                    .arg("test_data")
                    .exec_async(&mut connection)
                    .await?;
                let res: String = cmd("GET")
                    .arg("test")
                    .clone()
                    .query_async(&mut connection)
                    .await?;
                assert_eq!(res, "test_data");
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[test]
    fn test_async_cluster_io_error() {
        let name = "node";
        let completed = Arc::new(AtomicI32::new(0));
        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]).retries(2),
            name,
            move |cmd: &[u8], port| {
                respond_startup_two_nodes(name, cmd)?;
                // Error twice with io-error, ensure connection is reestablished w/out calling
                // other node (i.e., not doing a full slot rebuild)
                match port {
                    6380 => panic!("Node should not be called"),
                    _ => match completed.fetch_add(1, Ordering::SeqCst) {
                        0..=1 => Err(Err(RedisError::from(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "mock-io-error",
                        )))),
                        _ => Err(Ok(Value::BulkString(b"123".to_vec()))),
                    },
                }
            },
        );

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        assert_eq!(value, Ok(Some(123)));
    }

    #[test]
    fn test_async_cluster_non_retryable_error_should_not_retry() {
        let name = "node";
        let completed = Arc::new(AtomicI32::new(0));
        let MockEnv {
            async_connection: mut connection,
            handler: _handler,
            runtime,
            ..
        } = MockEnv::new(name, {
            let completed = completed.clone();
            move |cmd: &[u8], _| {
                respond_startup_two_nodes(name, cmd)?;
                // Error twice with io-error, ensure connection is reestablished w/out calling
                // other node (i.e., not doing a full slot rebuild)
                completed.fetch_add(1, Ordering::SeqCst);
                Err(Err((ErrorKind::ReadOnly, "").into()))
            }
        });

        let value = runtime.block_on(
            cmd("GET")
                .arg("test")
                .query_async::<Option<i32>>(&mut connection),
        );

        match value {
            Ok(_) => panic!("result should be an error"),
            Err(e) => match e.kind() {
                ErrorKind::ReadOnly => {}
                _ => panic!("Expected ReadOnly but got {:?}", e.kind()),
            },
        }
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_async_cluster_can_be_created_with_partial_slot_coverage() {
        let name = "test_async_cluster_can_be_created_with_partial_slot_coverage";
        let slots_config = Some(vec![
            MockSlotRange {
                primary_port: 6379,
                replica_ports: vec![],
                slot_range: (0..8000),
            },
            MockSlotRange {
                primary_port: 6381,
                replica_ports: vec![],
                slot_range: (8201..16380),
            },
        ]);

        let MockEnv {
            async_connection: mut connection,
            handler: _handler,
            runtime,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(0)
                .read_from_replicas(),
            name,
            move |received_cmd: &[u8], _| {
                respond_startup_with_replica_using_config(
                    name,
                    received_cmd,
                    slots_config.clone(),
                )?;
                Err(Ok(Value::SimpleString("PONG".into())))
            },
        );

        let res = runtime.block_on(connection.req_packed_command(&redis::cmd("PING")));
        assert!(res.is_ok());
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_handle_complete_server_disconnect_without_panicking(
        #[case] runtime: RuntimeType,
    ) {
        let cluster =
            TestClusterContext::new_with_cluster_client_builder(|builder| builder.retries(2));
        block_on_all(
            async move {
                let mut connection = cluster.async_connection().await;
                drop(cluster);
                for _ in 0..5 {
                    let cmd = cmd("PING");
                    let result = connection
                        .route_command(&cmd, RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random))
                        .await;
                    // TODO - this should be a NoConnectionError, but ATM we get the errors from the failing
                    assert!(result.is_err());
                    // This will route to all nodes - different path through the code.
                    let result = connection.req_packed_command(&cmd).await;
                    // TODO - this should be a NoConnectionError, but ATM we get the errors from the failing
                    assert!(result.is_err());
                }
                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_reconnect_after_complete_server_disconnect(#[case] runtime: RuntimeType) {
        let cluster = TestClusterContext::new_insecure_with_cluster_client_builder(|builder| {
            builder.retries(2)
        });

        block_on_all(
            async move {
                let ports: Vec<_> = cluster
                    .nodes
                    .iter()
                    .map(|info| match info.addr {
                        redis::ConnectionAddr::Tcp(_, port) => port,
                        redis::ConnectionAddr::TcpTls { port, .. } => port,
                        redis::ConnectionAddr::Unix(_) => {
                            panic!("no unix sockets in cluster tests")
                        }
                    })
                    .collect();

                let mut connection = cluster.async_connection().await;
                drop(cluster);

                let cmd = cmd("PING");

                let result = connection
                    .route_command(&cmd, RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random))
                    .await;
                // TODO - this should be a NoConnectionError, but ATM we get the errors from the failing
                assert!(result.is_err());

                // This will route to all nodes - different path through the code.
                let result = connection.req_packed_command(&cmd).await;
                // TODO - this should be a NoConnectionError, but ATM we get the errors from the failing
                assert!(result.is_err());

                let _cluster = RedisCluster::new(RedisClusterConfiguration {
                    ports: ports.clone(),
                    ..Default::default()
                });

                let result = connection.req_packed_command(&cmd).await.unwrap();
                assert_eq!(result, Value::SimpleString("PONG".to_string()));

                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_async_cluster_reconnect_after_complete_server_disconnect_route_to_many(
        #[case] runtime: RuntimeType,
    ) {
        let cluster = TestClusterContext::new_insecure_with_cluster_client_builder(|builder| {
            builder.retries(3)
        });

        block_on_all(
            async move {
                let ports: Vec<_> = cluster
                    .nodes
                    .iter()
                    .map(|info| match info.addr {
                        redis::ConnectionAddr::Tcp(_, port) => port,
                        redis::ConnectionAddr::TcpTls { port, .. } => port,
                        redis::ConnectionAddr::Unix(_) => {
                            panic!("no unix sockets in cluster tests")
                        }
                    })
                    .collect();

                let mut connection = cluster.async_connection().await;
                drop(cluster);

                // recreate cluster
                let _cluster = RedisCluster::new(RedisClusterConfiguration {
                    ports: ports.clone(),
                    ..Default::default()
                });

                let cmd = cmd("PING");
                // explicitly route to all primaries and request all succeeded
                let result = connection
                    .route_command(
                        &cmd,
                        RoutingInfo::MultiNode((
                            MultipleNodeRoutingInfo::AllMasters,
                            Some(redis::cluster_routing::ResponsePolicy::AllSucceeded),
                        )),
                    )
                    .await
                    .unwrap();
                assert_eq!(result, Value::SimpleString("PONG".to_string()));

                Ok::<_, RedisError>(())
            },
            runtime,
        )
        .unwrap();
    }

    #[test]
    fn test_async_cluster_saves_reconnected_connection() {
        let name = "test_async_cluster_saves_reconnected_connection";
        let ping_attempts = Arc::new(AtomicI32::new(0));
        let ping_attempts_clone = ping_attempts.clone();
        let get_attempts = AtomicI32::new(0);

        let MockEnv {
            runtime,
            async_connection: mut connection,
            handler: _handler,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")]).retries(1),
            name,
            move |cmd: &[u8], port| {
                if port == 6380 {
                    respond_startup_two_nodes(name, cmd)?;
                    return Err(parse_redis_value(
                        format!("-MOVED 123 {name}:6379\r\n").as_bytes(),
                    ));
                }

                if contains_slice(cmd, b"PING") {
                    let connect_attempt = ping_attempts_clone.fetch_add(1, Ordering::Relaxed);
                    let past_get_attempts = get_attempts.load(Ordering::Relaxed);
                    // We want connection checks to fail after the first GET attempt, until it retries. Hence, we wait for 5 PINGs -
                    // 1. initial connection,
                    // 2. refresh slots on client creation,
                    // 3. refresh_connections `check_connection` after first GET failed,
                    // 4. refresh_connections `connect_and_check` after first GET failed,
                    // 5. reconnect on 2nd GET attempt.
                    // more than 5 attempts mean that the server reconnects more than once, which is the behavior we're testing against.
                    if past_get_attempts != 1 || connect_attempt > 3 {
                        respond_startup_two_nodes(name, cmd)?;
                    }
                    if connect_attempt > 5 {
                        panic!("Too many pings!");
                    }
                    Err(Err(broken_pipe_error()))
                } else {
                    respond_startup_two_nodes(name, cmd)?;
                    let past_get_attempts = get_attempts.fetch_add(1, Ordering::Relaxed);
                    // we fail the initial GET request, and after that we'll fail the first reconnect attempt, in the `refresh_connections` attempt.
                    if past_get_attempts == 0 {
                        // Error once with io-error, ensure connection is reestablished w/out calling
                        // other node (i.e., not doing a full slot rebuild)
                        Err(Err(broken_pipe_error()))
                    } else {
                        Err(Ok(Value::BulkString(b"123".to_vec())))
                    }
                }
            },
        );

        for _ in 0..4 {
            let value = runtime.block_on(
                cmd("GET")
                    .arg("test")
                    .query_async::<Option<i32>>(&mut connection),
            );

            assert_eq!(value, Ok(Some(123)));
        }
        // If you need to change the number here due to a change in the cluster, you probably also need to adjust the test.
        // See the PING counts above to explain why 5 is the target number.
        assert_eq!(ping_attempts.load(Ordering::Acquire), 5);
    }

    #[rstest]
    #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
    #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
    #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
    fn test_kill_connection_on_drop_even_when_blocking(#[case] runtime: RuntimeType) {
        let ctx = TestClusterContext::new_with_cluster_client_builder(|builder| builder.retries(3));

        block_on_all(
            async move {
                async fn count_ids(
                    conn: &mut impl redis::aio::ConnectionLike,
                ) -> RedisResult<usize> {
                    // we use a pipeline with a fake command in order to ensure that the CLIENT LIST command gets routed to the correct node.
                    // we use LIST as the key, in order to ensure that adding CLIENT LIST doesn't trigger a CROSSSLOTS error.
                    let initial_connections: String = pipe()
                        .cmd("GET")
                        .arg("LIST")
                        .cmd("CLIENT")
                        .arg("LIST")
                        .query_async::<Vec<Option<String>>>(conn)
                        .await?
                        .pop()
                        .unwrap()
                        .unwrap();

                    Ok(initial_connections
                        .as_bytes()
                        .windows(3)
                        .filter(|substr| substr == b"id=")
                        .count())
                }

                let mut conn = ctx.async_connection().await;
                let mut connection_to_dispose_of = ctx.async_connection().await;

                assert_eq!(count_ids(&mut conn).await.unwrap(), 2);

                let mut cmd = cmd("BLPOP");
                let command_that_blocks = Box::pin(async move {
                    () = cmd
                        .arg("LIST")
                        .arg(0)
                        .exec_async(&mut connection_to_dispose_of)
                        .await
                        .unwrap();
                    unreachable!("This shouldn't happen");
                })
                .fuse();
                let timeout =
                    futures_time::task::sleep(futures_time::time::Duration::from_millis(1)).fuse();

                let others = futures::future::select(command_that_blocks, timeout).await;
                drop(others);

                futures_time::task::sleep(futures_time::time::Duration::from_millis(100)).await;

                assert_eq!(count_ids(&mut conn).await.unwrap(), 1);

                Ok(())
            },
            runtime,
        )
        .unwrap();
    }

    #[test]
    fn test_async_cluster_do_not_retry_when_receiver_was_dropped() {
        let name = "test_async_cluster_do_not_retry_when_receiver_was_dropped";
        let cmd = cmd("FAKE_COMMAND");
        let packed_cmd = cmd.get_packed_command();
        let request_counter = Arc::new(AtomicU32::new(0));
        let cloned_req_counter = request_counter.clone();
        let MockEnv {
            runtime,
            async_connection: mut connection,
            ..
        } = MockEnv::with_client_builder(
            ClusterClient::builder(vec![&*format!("redis://{name}")])
                .retries(5)
                .max_retry_wait(2)
                .min_retry_wait(2),
            name,
            move |received_cmd: &[u8], _| {
                respond_startup(name, received_cmd)?;

                if received_cmd == packed_cmd {
                    cloned_req_counter.fetch_add(1, Ordering::Relaxed);
                    return Err(Err((ErrorKind::TryAgain, "seriously, try again").into()));
                }

                Err(Ok(Value::Okay))
            },
        );

        runtime.block_on(async move {
            let err = cmd
                .exec_async(&mut connection)
                .timeout(futures_time::time::Duration::from_millis(1))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

            // we sleep here, to allow the cluster connection time to retry. We expect it won't, but without this
            // sleep the test will complete before the the runtime gave the connection time to retry, which would've made the
            // test pass regardless of whether the connection tries retrying or not.
            sleep(Duration::from_millis(10).into()).await;
        });

        assert_eq!(request_counter.load(Ordering::Relaxed), 1);
    }

    mod pubsub {
        use redis::{cluster_async::ClusterConnection, PushInfo, PushKind};
        use tokio::join;

        use super::*;

        async fn check_if_redis_6(conn: &mut ClusterConnection) -> bool {
            let response = conn
                .route_command(
                    cmd("INFO").arg("server"),
                    RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random),
                )
                .await
                .unwrap();
            let info = from_owned_redis_value::<InfoDict>(response).unwrap();
            parse_version(info).0 == 6
        }

        async fn subscribe_to_channels(
            pubsub_conn: &mut ClusterConnection,
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<PushInfo>,
            is_redis_6: bool,
        ) -> RedisResult<()> {
            let _: () = pubsub_conn.subscribe("regular-phonewave").await?;
            let push: PushInfo = rx.recv().await.unwrap();
            assert_eq!(
                push,
                PushInfo {
                    kind: PushKind::Subscribe,
                    data: vec![
                        Value::BulkString(b"regular-phonewave".to_vec()),
                        Value::Int(1)
                    ]
                }
            );

            let _: () = pubsub_conn.psubscribe("phonewave*").await?;
            let push = rx.recv().await.unwrap();
            assert_eq!(
                push,
                PushInfo {
                    kind: PushKind::PSubscribe,
                    data: vec![Value::BulkString(b"phonewave*".to_vec()), Value::Int(2)]
                }
            );

            if !is_redis_6 {
                let _: () = pubsub_conn.ssubscribe("sphonewave").await?;
                let push = rx.recv().await.unwrap();
                assert_eq!(
                    push,
                    PushInfo {
                        kind: PushKind::SSubscribe,
                        data: vec![Value::BulkString(b"sphonewave".to_vec()), Value::Int(1)]
                    }
                );
            }

            Ok(())
        }

        async fn check_publishing(
            publish_conn: &mut ClusterConnection,
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<PushInfo>,
            is_redis_6: bool,
        ) -> RedisResult<()> {
            let _: () = publish_conn.publish("regular-phonewave", "banana").await?;
            let push = rx.recv().await.unwrap();
            assert_eq!(
                push,
                PushInfo {
                    kind: PushKind::Message,
                    data: vec![
                        Value::BulkString(b"regular-phonewave".to_vec()),
                        Value::BulkString(b"banana".to_vec()),
                    ]
                }
            );

            let _: () = publish_conn.publish("phonewave-pattern", "banana").await?;
            let push = rx.recv().await.unwrap();
            assert_eq!(
                push,
                PushInfo {
                    kind: PushKind::PMessage,
                    data: vec![
                        Value::BulkString(b"phonewave*".to_vec()),
                        Value::BulkString(b"phonewave-pattern".to_vec()),
                        Value::BulkString(b"banana".to_vec()),
                    ]
                }
            );

            if !is_redis_6 {
                let _: () = publish_conn.spublish("sphonewave", "banana").await?;
                let push = rx.recv().await.unwrap();
                assert_eq!(
                    push,
                    PushInfo {
                        kind: PushKind::SMessage,
                        data: vec![
                            Value::BulkString(b"sphonewave".to_vec()),
                            Value::BulkString(b"banana".to_vec()),
                        ]
                    }
                );
            }
            Ok(())
        }

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        fn pub_sub_subscription(#[case] runtime: RuntimeType) {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = TestClusterContext::new_with_cluster_client_builder(|builder| {
                builder
                    .use_protocol(ProtocolVersion::RESP3)
                    .push_sender(tx.clone())
            });

            block_on_all(
                async move {
                    let (mut publish_conn, mut pubsub_conn) =
                        join!(ctx.async_connection(), ctx.async_connection());
                    let is_redis_6 = check_if_redis_6(&mut pubsub_conn).await;

                    subscribe_to_channels(&mut pubsub_conn, &mut rx, is_redis_6).await?;

                    check_publishing(&mut publish_conn, &mut rx, is_redis_6).await?;

                    Ok::<_, RedisError>(())
                },
                runtime,
            )
            .unwrap();
        }

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        fn pub_sub_subscription_with_config(#[case] runtime: RuntimeType) {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = TestClusterContext::new_with_cluster_client_builder(|builder| {
                builder.use_protocol(ProtocolVersion::RESP3)
            });
            let config = redis::cluster::ClusterConfig::new().set_push_sender(tx.clone());
            block_on_all(
                async move {
                    let (mut publish_conn, mut pubsub_conn) = join!(
                        ctx.async_connection_with_config(config.clone()),
                        ctx.async_connection_with_config(config)
                    );
                    let is_redis_6 = check_if_redis_6(&mut pubsub_conn).await;

                    subscribe_to_channels(&mut pubsub_conn, &mut rx, is_redis_6).await?;

                    check_publishing(&mut publish_conn, &mut rx, is_redis_6).await?;

                    Ok::<_, RedisError>(())
                },
                runtime,
            )
            .unwrap();
        }

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        fn pub_sub_shardnumsub(#[case] runtime: RuntimeType) {
            let ctx = TestClusterContext::new_with_cluster_client_builder(|builder| {
                builder.use_protocol(ProtocolVersion::RESP3)
            });

            block_on_all(
                async move {
                    let mut pubsub_conn = ctx.async_connection().await;
                    if check_if_redis_6(&mut pubsub_conn).await {
                        return Ok(());
                    }

                    let _: () = pubsub_conn.ssubscribe("foo").await?;

                    let res = cmd("pubsub")
                        .arg("SHARDNUMSUB")
                        .arg("foo")
                        .query_async::<(String, usize)>(&mut pubsub_conn)
                        .await
                        .unwrap();
                    assert_eq!(res, ("foo".to_string(), 1));

                    Ok::<_, RedisError>(())
                },
                runtime,
            )
            .unwrap();
        }

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        fn pub_sub_unsubscription(#[case] runtime: RuntimeType) {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = TestClusterContext::new_with_cluster_client_builder(|builder| {
                builder
                    .use_protocol(ProtocolVersion::RESP3)
                    .push_sender(tx.clone())
            });

            block_on_all(
                async move {
                    let (mut publish_conn, mut pubsub_conn) =
                        join!(ctx.async_connection(), ctx.async_connection());
                    let is_redis_6 = check_if_redis_6(&mut pubsub_conn).await;

                    let _: () = pubsub_conn.subscribe("regular-phonewave").await?;
                    let push = rx.recv().await.unwrap();
                    assert_eq!(
                        push,
                        PushInfo {
                            kind: PushKind::Subscribe,
                            data: vec![
                                Value::BulkString(b"regular-phonewave".to_vec()),
                                Value::Int(1)
                            ]
                        }
                    );
                    let _: () = pubsub_conn.unsubscribe("regular-phonewave").await?;
                    let push = rx.recv().await.unwrap();
                    assert_eq!(
                        push,
                        PushInfo {
                            kind: PushKind::Unsubscribe,
                            data: vec![
                                Value::BulkString(b"regular-phonewave".to_vec()),
                                Value::Int(0)
                            ]
                        }
                    );

                    let _: () = pubsub_conn.psubscribe("phonewave*").await?;
                    let push = rx.recv().await.unwrap();
                    assert_eq!(
                        push,
                        PushInfo {
                            kind: PushKind::PSubscribe,
                            data: vec![Value::BulkString(b"phonewave*".to_vec()), Value::Int(1)]
                        }
                    );
                    let _: () = pubsub_conn.punsubscribe("phonewave*").await?;
                    let push = rx.recv().await.unwrap();
                    assert_eq!(
                        push,
                        PushInfo {
                            kind: PushKind::PUnsubscribe,
                            data: vec![Value::BulkString(b"phonewave*".to_vec()), Value::Int(0)]
                        }
                    );

                    if !is_redis_6 {
                        let _: () = pubsub_conn.ssubscribe("sphonewave").await?;
                        let push = rx.recv().await.unwrap();
                        assert_eq!(
                            push,
                            PushInfo {
                                kind: PushKind::SSubscribe,
                                data: vec![
                                    Value::BulkString(b"sphonewave".to_vec()),
                                    Value::Int(1)
                                ]
                            }
                        );
                        let _: () = pubsub_conn.sunsubscribe("sphonewave").await?;
                        let push = rx.recv().await.unwrap();
                        assert_eq!(
                            push,
                            PushInfo {
                                kind: PushKind::SUnsubscribe,
                                data: vec![
                                    Value::BulkString(b"sphonewave".to_vec()),
                                    Value::Int(0)
                                ]
                            }
                        );
                    }

                    let _: () = publish_conn.publish("regular-phonewave", "banana").await?;
                    let _: () = publish_conn.publish("phonewave-pattern", "banana").await?;
                    if !is_redis_6 {
                        let _: () = publish_conn.spublish("sphonewave", "banana").await?;
                    }

                    assert_eq!(
                        rx.try_recv(),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                    );

                    Ok::<_, RedisError>(())
                },
                runtime,
            )
            .unwrap();
        }

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        fn connection_is_still_usable_if_pubsub_receiver_is_dropped(#[case] runtime: RuntimeType) {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = TestClusterContext::new_with_cluster_client_builder(|builder| {
                builder
                    .use_protocol(ProtocolVersion::RESP3)
                    .push_sender(tx.clone())
            });

            block_on_all(
                async move {
                    let mut pubsub_conn = ctx.async_connection().await;
                    let is_redis_6 = check_if_redis_6(&mut pubsub_conn).await;

                    subscribe_to_channels(&mut pubsub_conn, &mut rx, is_redis_6).await?;

                    drop(rx);

                    assert_eq!(
                        cmd("PING")
                            .query_async::<String>(&mut pubsub_conn)
                            .await
                            .unwrap(),
                        "PONG".to_string()
                    );

                    Ok::<_, RedisError>(())
                },
                runtime,
            )
            .unwrap();
        }

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        fn multiple_subscribes_and_unsubscribes_work(#[case] runtime: RuntimeType) {
            // In this test we subscribe on all subscription variations to 3 channels in a single call, then unsubscribe from 2 channels.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = TestClusterContext::new_with_cluster_client_builder(|builder| {
                builder
                    .use_protocol(ProtocolVersion::RESP3)
                    .push_sender(tx.clone())
            });

            block_on_all(
                async move {
                    let mut pubsub_conn = ctx.async_connection().await;
                    let is_redis_6 = check_if_redis_6(&mut pubsub_conn).await;

                    let _: () = pubsub_conn
                        .subscribe(&[
                            "regular-phonewave1",
                            "regular-phonewave2",
                            "regular-phonewave3",
                        ])
                        .await?;
                    for i in 1..4 {
                        let push = rx.recv().await.unwrap();
                        assert_eq!(
                            push,
                            PushInfo {
                                kind: PushKind::Subscribe,
                                data: vec![
                                    Value::BulkString(
                                        format!("regular-phonewave{i}").as_bytes().to_vec()
                                    ),
                                    Value::Int(i)
                                ]
                            }
                        );
                    }
                    let _: () = pubsub_conn
                        .unsubscribe(&["regular-phonewave1", "regular-phonewave2"])
                        .await?;
                    for i in 1..3 {
                        let push = rx.recv().await.unwrap();
                        assert_eq!(
                            push,
                            PushInfo {
                                kind: PushKind::Unsubscribe,
                                data: vec![
                                    Value::BulkString(
                                        format!("regular-phonewave{i}").as_bytes().to_vec()
                                    ),
                                    Value::Int(3 - i)
                                ]
                            }
                        );
                    }

                    let _: () = pubsub_conn
                        .psubscribe(&["phonewave*1", "phonewave*2", "phonewave*3"])
                        .await?;
                    for i in 1..4 {
                        let push = rx.recv().await.unwrap();
                        assert_eq!(
                            push,
                            PushInfo {
                                kind: PushKind::PSubscribe,
                                data: vec![
                                    Value::BulkString(format!("phonewave*{i}").as_bytes().to_vec()),
                                    Value::Int(i)
                                ]
                            }
                        );
                    }

                    let _: () = pubsub_conn
                        .punsubscribe(&["phonewave*1", "phonewave*2"])
                        .await?;
                    for i in 1..3 {
                        let push = rx.recv().await.unwrap();
                        assert_eq!(
                            push,
                            PushInfo {
                                kind: PushKind::PUnsubscribe,
                                data: vec![
                                    Value::BulkString(format!("phonewave*{i}").as_bytes().to_vec()),
                                    Value::Int(3 - i)
                                ]
                            }
                        );
                    }
                    if !is_redis_6 {
                        // we use the curly braces in order to avoid cross slots errors.
                        let _: () = pubsub_conn
                            .ssubscribe(&["{sphonewave}1", "{sphonewave}2", "{sphonewave}3"])
                            .await?;
                        for i in 1..4 {
                            let push = rx.recv().await.unwrap();
                            assert_eq!(
                                push,
                                PushInfo {
                                    kind: PushKind::SSubscribe,
                                    data: vec![
                                        Value::BulkString(
                                            format!("{{sphonewave}}{i}").as_bytes().to_vec()
                                        ),
                                        Value::Int(i)
                                    ]
                                }
                            );
                        }

                        let _: () = pubsub_conn
                            .sunsubscribe(&["{sphonewave}1", "{sphonewave}2"])
                            .await?;
                        for i in 1..3 {
                            let push = rx.recv().await.unwrap();
                            assert_eq!(
                                push,
                                PushInfo {
                                    kind: PushKind::SUnsubscribe,
                                    data: vec![
                                        Value::BulkString(
                                            format!("{{sphonewave}}{i}").as_bytes().to_vec()
                                        ),
                                        Value::Int(3 - i)
                                    ]
                                }
                            );
                        }
                    }

                    assert_eq!(
                        rx.try_recv(),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                    );

                    Ok::<_, RedisError>(())
                },
                runtime,
            )
            .unwrap();
        }

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        fn pub_sub_reconnect_after_disconnect(#[case] runtime: RuntimeType) {
            // in this test we will subscribe to channels, then restart the server, and check that the connection
            // doesn't send disconnect message, but instead resubscribes automatically.

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = TestClusterContext::new_insecure_with_cluster_client_builder(|builder| {
                builder
                    .use_protocol(ProtocolVersion::RESP3)
                    .push_sender(tx.clone())
            });

            block_on_all(
                async move {
                    let ports: Vec<_> = ctx
                        .nodes
                        .iter()
                        .map(|info| match info.addr {
                            redis::ConnectionAddr::Tcp(_, port) => port,
                            redis::ConnectionAddr::TcpTls { port, .. } => port,
                            redis::ConnectionAddr::Unix(_) => {
                                panic!("no unix sockets in cluster tests")
                            }
                        })
                        .collect();

                    let (mut publish_conn, mut pubsub_conn) =
                        join!(ctx.async_connection(), ctx.async_connection());
                    let is_redis_6 = check_if_redis_6(&mut pubsub_conn).await;

                    subscribe_to_channels(&mut pubsub_conn, &mut rx, is_redis_6).await?;

                    println!("dropped");
                    drop(ctx);

                    // we expect 1 disconnect per connection to node. 2 connections * 3 node = 6 disconnects.
                    for _ in 0..6 {
                        let push = rx.recv().await.unwrap();
                        assert_eq!(
                            push,
                            PushInfo {
                                kind: PushKind::Disconnection,
                                data: vec![]
                            }
                        );
                    }

                    // recreate cluster
                    let _cluster = RedisCluster::new(RedisClusterConfiguration {
                        ports: ports.clone(),
                        ..Default::default()
                    });

                    // verify that we didn't get any disconnect notices.
                    assert_eq!(
                        rx.try_recv(),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                    );

                    // send request to trigger reconnection.
                    let cmd = cmd("PING");
                    let _ = pubsub_conn
                        .route_command(
                            &cmd,
                            RoutingInfo::MultiNode((
                                MultipleNodeRoutingInfo::AllMasters,
                                Some(redis::cluster_routing::ResponsePolicy::AllSucceeded),
                            )),
                        )
                        .await?;

                    // the resubsriptions can be received in any order, so we assert without assuming order.
                    let mut pushes = Vec::new();
                    pushes.push(rx.recv().await.unwrap());
                    pushes.push(rx.recv().await.unwrap());
                    if !is_redis_6 {
                        pushes.push(rx.recv().await.unwrap());
                    }
                    // we expect only 3 resubscriptions.
                    assert!(rx.try_recv().is_err());
                    assert!(pushes.contains(&PushInfo {
                        kind: PushKind::Subscribe,
                        data: vec![
                            Value::BulkString(b"regular-phonewave".to_vec()),
                            Value::Int(1)
                        ]
                    }));
                    assert!(pushes.contains(&PushInfo {
                        kind: PushKind::PSubscribe,
                        data: vec![Value::BulkString(b"phonewave*".to_vec()), Value::Int(2)]
                    }));

                    if !is_redis_6 {
                        assert!(pushes.contains(&PushInfo {
                            kind: PushKind::SSubscribe,
                            data: vec![Value::BulkString(b"sphonewave".to_vec()), Value::Int(1)]
                        }));
                    }

                    check_publishing(&mut publish_conn, &mut rx, is_redis_6).await?;

                    Ok::<_, RedisError>(())
                },
                runtime,
            )
            .unwrap();
        }
    }

    #[cfg(feature = "tls-rustls")]
    mod mtls_test {
        use crate::support::mtls_test::create_cluster_client_from_cluster;
        use redis::ConnectionInfo;

        use super::*;

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
        fn test_async_cluster_basic_cmd_with_mtls(#[case] runtime: RuntimeType) {
            let cluster = TestClusterContext::new_with_mtls();
            block_on_all(
                async move {
                    let client = create_cluster_client_from_cluster(&cluster, true).unwrap();
                    let mut connection = client.get_async_connection().await.unwrap();
                    cmd("SET")
                        .arg("test")
                        .arg("test_data")
                        .exec_async(&mut connection)
                        .await?;
                    let res: String = cmd("GET")
                        .arg("test")
                        .clone()
                        .query_async(&mut connection)
                        .await?;
                    assert_eq!(res, "test_data");
                    Ok::<_, RedisError>(())
                },
                runtime,
            )
            .unwrap();
        }

        #[rstest]
        #[cfg_attr(feature = "tokio-comp", case::tokio(RuntimeType::Tokio))]
        #[cfg_attr(feature = "async-std-comp", case::async_std(RuntimeType::AsyncStd))]
        #[cfg_attr(feature = "smol-comp", case::smol(RuntimeType::Smol))]
        fn test_async_cluster_should_not_connect_without_mtls_enabled(
            #[case] runtime: RuntimeType,
        ) {
            let cluster = TestClusterContext::new_with_mtls();
            block_on_all(async move {
            let client = create_cluster_client_from_cluster(&cluster, false).unwrap();
            let connection = client.get_async_connection().await;
            match cluster.cluster.servers.first().unwrap().connection_info() {
                ConnectionInfo {
                    addr: redis::ConnectionAddr::TcpTls { .. },
                    ..
            } => {
                if connection.is_ok() {
                    panic!("Must NOT be able to connect without client credentials if server accepts TLS");
                }
            }
            _ => {
                if let Err(e) = connection {
                    panic!("Must be able to connect without client credentials if server does NOT accept TLS: {e:?}");
                }
            }
            }
            Ok::<_, RedisError>(())
        },
            runtime,).unwrap();
        }
    }
}
