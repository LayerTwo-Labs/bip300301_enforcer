// @generated
/// Generated client implementations.
pub mod validator_service_client {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct ValidatorServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl ValidatorServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> ValidatorServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::Body>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + std::marker::Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + std::marker::Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> ValidatorServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::Body>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::Body>,
            >>::Error: Into<StdError> + std::marker::Send + std::marker::Sync,
        {
            ValidatorServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        pub async fn get_block_header_info(
            &mut self,
            request: impl tonic::IntoRequest<super::GetBlockHeaderInfoRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBlockHeaderInfoResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetBlockHeaderInfo",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.ValidatorService",
                        "GetBlockHeaderInfo",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_block_info(
            &mut self,
            request: impl tonic::IntoRequest<super::GetBlockInfoRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBlockInfoResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetBlockInfo",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.ValidatorService", "GetBlockInfo"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_bmm_h_star_commitment(
            &mut self,
            request: impl tonic::IntoRequest<super::GetBmmHStarCommitmentRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBmmHStarCommitmentResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetBmmHStarCommitment",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.ValidatorService",
                        "GetBmmHStarCommitment",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_chain_info(
            &mut self,
            request: impl tonic::IntoRequest<super::GetChainInfoRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetChainInfoResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetChainInfo",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.ValidatorService", "GetChainInfo"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_chain_tip(
            &mut self,
            request: impl tonic::IntoRequest<super::GetChainTipRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetChainTipResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetChainTip",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.ValidatorService", "GetChainTip"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_coinbase_psbt(
            &mut self,
            request: impl tonic::IntoRequest<super::GetCoinbasePsbtRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetCoinbasePsbtResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetCoinbasePSBT",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.ValidatorService",
                        "GetCoinbasePSBT",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_ctip(
            &mut self,
            request: impl tonic::IntoRequest<super::GetCtipRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetCtipResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetCtip",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.ValidatorService", "GetCtip"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_sidechain_proposals(
            &mut self,
            request: impl tonic::IntoRequest<super::GetSidechainProposalsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetSidechainProposalsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetSidechainProposals",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.ValidatorService",
                        "GetSidechainProposals",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_sidechains(
            &mut self,
            request: impl tonic::IntoRequest<super::GetSidechainsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetSidechainsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetSidechains",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.ValidatorService",
                        "GetSidechains",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_two_way_peg_data(
            &mut self,
            request: impl tonic::IntoRequest<super::GetTwoWayPegDataRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetTwoWayPegDataResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/GetTwoWayPegData",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.ValidatorService",
                        "GetTwoWayPegData",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn subscribe_events(
            &mut self,
            request: impl tonic::IntoRequest<super::SubscribeEventsRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::SubscribeEventsResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/SubscribeEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.ValidatorService",
                        "SubscribeEvents",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn subscribe_header_sync_progress(
            &mut self,
            request: impl tonic::IntoRequest<super::SubscribeHeaderSyncProgressRequest>,
        ) -> std::result::Result<
            tonic::Response<
                tonic::codec::Streaming<super::SubscribeHeaderSyncProgressResponse>,
            >,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/SubscribeHeaderSyncProgress",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.ValidatorService",
                        "SubscribeHeaderSyncProgress",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn stop(
            &mut self,
            request: impl tonic::IntoRequest<super::StopRequest>,
        ) -> std::result::Result<tonic::Response<super::StopResponse>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.ValidatorService/Stop",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("cusf.mainchain.v1.ValidatorService", "Stop"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod validator_service_server {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with ValidatorServiceServer.
    #[async_trait]
    pub trait ValidatorService: std::marker::Send + std::marker::Sync + 'static {
        async fn get_block_header_info(
            &self,
            request: tonic::Request<super::GetBlockHeaderInfoRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBlockHeaderInfoResponse>,
            tonic::Status,
        >;
        async fn get_block_info(
            &self,
            request: tonic::Request<super::GetBlockInfoRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBlockInfoResponse>,
            tonic::Status,
        >;
        async fn get_bmm_h_star_commitment(
            &self,
            request: tonic::Request<super::GetBmmHStarCommitmentRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBmmHStarCommitmentResponse>,
            tonic::Status,
        >;
        async fn get_chain_info(
            &self,
            request: tonic::Request<super::GetChainInfoRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetChainInfoResponse>,
            tonic::Status,
        >;
        async fn get_chain_tip(
            &self,
            request: tonic::Request<super::GetChainTipRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetChainTipResponse>,
            tonic::Status,
        >;
        async fn get_coinbase_psbt(
            &self,
            request: tonic::Request<super::GetCoinbasePsbtRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetCoinbasePsbtResponse>,
            tonic::Status,
        >;
        async fn get_ctip(
            &self,
            request: tonic::Request<super::GetCtipRequest>,
        ) -> std::result::Result<tonic::Response<super::GetCtipResponse>, tonic::Status>;
        async fn get_sidechain_proposals(
            &self,
            request: tonic::Request<super::GetSidechainProposalsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetSidechainProposalsResponse>,
            tonic::Status,
        >;
        async fn get_sidechains(
            &self,
            request: tonic::Request<super::GetSidechainsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetSidechainsResponse>,
            tonic::Status,
        >;
        async fn get_two_way_peg_data(
            &self,
            request: tonic::Request<super::GetTwoWayPegDataRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetTwoWayPegDataResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the SubscribeEvents method.
        type SubscribeEventsStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::SubscribeEventsResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        async fn subscribe_events(
            &self,
            request: tonic::Request<super::SubscribeEventsRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::SubscribeEventsStream>,
            tonic::Status,
        >;
        /// Server streaming response type for the SubscribeHeaderSyncProgress method.
        type SubscribeHeaderSyncProgressStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<
                    super::SubscribeHeaderSyncProgressResponse,
                    tonic::Status,
                >,
            >
            + std::marker::Send
            + 'static;
        async fn subscribe_header_sync_progress(
            &self,
            request: tonic::Request<super::SubscribeHeaderSyncProgressRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::SubscribeHeaderSyncProgressStream>,
            tonic::Status,
        >;
        async fn stop(
            &self,
            request: tonic::Request<super::StopRequest>,
        ) -> std::result::Result<tonic::Response<super::StopResponse>, tonic::Status>;
    }
    #[derive(Debug)]
    pub struct ValidatorServiceServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T> ValidatorServiceServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for ValidatorServiceServer<T>
    where
        T: ValidatorService,
        B: Body + std::marker::Send + 'static,
        B::Error: Into<StdError> + std::marker::Send + 'static,
    {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            match req.uri().path() {
                "/cusf.mainchain.v1.ValidatorService/GetBlockHeaderInfo" => {
                    #[allow(non_camel_case_types)]
                    struct GetBlockHeaderInfoSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetBlockHeaderInfoRequest>
                    for GetBlockHeaderInfoSvc<T> {
                        type Response = super::GetBlockHeaderInfoResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetBlockHeaderInfoRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_block_header_info(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetBlockHeaderInfoSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetBlockInfo" => {
                    #[allow(non_camel_case_types)]
                    struct GetBlockInfoSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetBlockInfoRequest>
                    for GetBlockInfoSvc<T> {
                        type Response = super::GetBlockInfoResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetBlockInfoRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_block_info(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetBlockInfoSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetBmmHStarCommitment" => {
                    #[allow(non_camel_case_types)]
                    struct GetBmmHStarCommitmentSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetBmmHStarCommitmentRequest>
                    for GetBmmHStarCommitmentSvc<T> {
                        type Response = super::GetBmmHStarCommitmentResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetBmmHStarCommitmentRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_bmm_h_star_commitment(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetBmmHStarCommitmentSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetChainInfo" => {
                    #[allow(non_camel_case_types)]
                    struct GetChainInfoSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetChainInfoRequest>
                    for GetChainInfoSvc<T> {
                        type Response = super::GetChainInfoResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetChainInfoRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_chain_info(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetChainInfoSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetChainTip" => {
                    #[allow(non_camel_case_types)]
                    struct GetChainTipSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetChainTipRequest>
                    for GetChainTipSvc<T> {
                        type Response = super::GetChainTipResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetChainTipRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_chain_tip(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetChainTipSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetCoinbasePSBT" => {
                    #[allow(non_camel_case_types)]
                    struct GetCoinbasePSBTSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetCoinbasePsbtRequest>
                    for GetCoinbasePSBTSvc<T> {
                        type Response = super::GetCoinbasePsbtResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetCoinbasePsbtRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_coinbase_psbt(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetCoinbasePSBTSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetCtip" => {
                    #[allow(non_camel_case_types)]
                    struct GetCtipSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetCtipRequest>
                    for GetCtipSvc<T> {
                        type Response = super::GetCtipResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetCtipRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_ctip(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetCtipSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetSidechainProposals" => {
                    #[allow(non_camel_case_types)]
                    struct GetSidechainProposalsSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetSidechainProposalsRequest>
                    for GetSidechainProposalsSvc<T> {
                        type Response = super::GetSidechainProposalsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetSidechainProposalsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_sidechain_proposals(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetSidechainProposalsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetSidechains" => {
                    #[allow(non_camel_case_types)]
                    struct GetSidechainsSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetSidechainsRequest>
                    for GetSidechainsSvc<T> {
                        type Response = super::GetSidechainsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetSidechainsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_sidechains(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetSidechainsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/GetTwoWayPegData" => {
                    #[allow(non_camel_case_types)]
                    struct GetTwoWayPegDataSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::GetTwoWayPegDataRequest>
                    for GetTwoWayPegDataSvc<T> {
                        type Response = super::GetTwoWayPegDataResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetTwoWayPegDataRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::get_two_way_peg_data(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetTwoWayPegDataSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/SubscribeEvents" => {
                    #[allow(non_camel_case_types)]
                    struct SubscribeEventsSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::ServerStreamingService<
                        super::SubscribeEventsRequest,
                    > for SubscribeEventsSvc<T> {
                        type Response = super::SubscribeEventsResponse;
                        type ResponseStream = T::SubscribeEventsStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SubscribeEventsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::subscribe_events(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = SubscribeEventsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/SubscribeHeaderSyncProgress" => {
                    #[allow(non_camel_case_types)]
                    struct SubscribeHeaderSyncProgressSvc<T: ValidatorService>(
                        pub Arc<T>,
                    );
                    impl<
                        T: ValidatorService,
                    > tonic::server::ServerStreamingService<
                        super::SubscribeHeaderSyncProgressRequest,
                    > for SubscribeHeaderSyncProgressSvc<T> {
                        type Response = super::SubscribeHeaderSyncProgressResponse;
                        type ResponseStream = T::SubscribeHeaderSyncProgressStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::SubscribeHeaderSyncProgressRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::subscribe_header_sync_progress(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = SubscribeHeaderSyncProgressSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.ValidatorService/Stop" => {
                    #[allow(non_camel_case_types)]
                    struct StopSvc<T: ValidatorService>(pub Arc<T>);
                    impl<
                        T: ValidatorService,
                    > tonic::server::UnaryService<super::StopRequest> for StopSvc<T> {
                        type Response = super::StopResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::StopRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as ValidatorService>::stop(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = StopSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        let mut response = http::Response::new(
                            tonic::body::Body::default(),
                        );
                        let headers = response.headers_mut();
                        headers
                            .insert(
                                tonic::Status::GRPC_STATUS,
                                (tonic::Code::Unimplemented as i32).into(),
                            );
                        headers
                            .insert(
                                http::header::CONTENT_TYPE,
                                tonic::metadata::GRPC_CONTENT_TYPE,
                            );
                        Ok(response)
                    })
                }
            }
        }
    }
    impl<T> Clone for ValidatorServiceServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    /// Generated gRPC service name
    pub const SERVICE_NAME: &str = "cusf.mainchain.v1.ValidatorService";
    impl<T> tonic::server::NamedService for ValidatorServiceServer<T> {
        const NAME: &'static str = SERVICE_NAME;
    }
}
/// Generated client implementations.
pub mod wallet_service_client {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct WalletServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl WalletServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> WalletServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::Body>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + std::marker::Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + std::marker::Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> WalletServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::Body>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::Body>,
            >>::Error: Into<StdError> + std::marker::Send + std::marker::Sync,
        {
            WalletServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        pub async fn broadcast_withdrawal_bundle(
            &mut self,
            request: impl tonic::IntoRequest<super::BroadcastWithdrawalBundleRequest>,
        ) -> std::result::Result<
            tonic::Response<super::BroadcastWithdrawalBundleResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/BroadcastWithdrawalBundle",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.WalletService",
                        "BroadcastWithdrawalBundle",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_bmm_critical_data_transaction(
            &mut self,
            request: impl tonic::IntoRequest<
                super::CreateBmmCriticalDataTransactionRequest,
            >,
        ) -> std::result::Result<
            tonic::Response<super::CreateBmmCriticalDataTransactionResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/CreateBmmCriticalDataTransaction",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.WalletService",
                        "CreateBmmCriticalDataTransaction",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_deposit_transaction(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateDepositTransactionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateDepositTransactionResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/CreateDepositTransaction",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.WalletService",
                        "CreateDepositTransaction",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_new_address(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateNewAddressRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateNewAddressResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/CreateNewAddress",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.WalletService",
                        "CreateNewAddress",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_sidechain_proposal(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateSidechainProposalRequest>,
        ) -> std::result::Result<
            tonic::Response<
                tonic::codec::Streaming<super::CreateSidechainProposalResponse>,
            >,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/CreateSidechainProposal",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.WalletService",
                        "CreateSidechainProposal",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn create_wallet(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateWalletRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateWalletResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/CreateWallet",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.WalletService", "CreateWallet"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_balance(
            &mut self,
            request: impl tonic::IntoRequest<super::GetBalanceRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBalanceResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/GetBalance",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.WalletService", "GetBalance"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_sidechain_deposit_transactions(
            &mut self,
            request: impl tonic::IntoRequest<
                super::ListSidechainDepositTransactionsRequest,
            >,
        ) -> std::result::Result<
            tonic::Response<super::ListSidechainDepositTransactionsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/ListSidechainDepositTransactions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.WalletService",
                        "ListSidechainDepositTransactions",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_transactions(
            &mut self,
            request: impl tonic::IntoRequest<super::ListTransactionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListTransactionsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/ListTransactions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.WalletService",
                        "ListTransactions",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_unspent_outputs(
            &mut self,
            request: impl tonic::IntoRequest<super::ListUnspentOutputsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListUnspentOutputsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/ListUnspentOutputs",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "cusf.mainchain.v1.WalletService",
                        "ListUnspentOutputs",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_info(
            &mut self,
            request: impl tonic::IntoRequest<super::GetInfoRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetInfoResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/GetInfo",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("cusf.mainchain.v1.WalletService", "GetInfo"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn send_transaction(
            &mut self,
            request: impl tonic::IntoRequest<super::SendTransactionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SendTransactionResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/SendTransaction",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.WalletService", "SendTransaction"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn unlock_wallet(
            &mut self,
            request: impl tonic::IntoRequest<super::UnlockWalletRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UnlockWalletResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/UnlockWallet",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.WalletService", "UnlockWallet"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn generate_blocks(
            &mut self,
            request: impl tonic::IntoRequest<super::GenerateBlocksRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::GenerateBlocksResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cusf.mainchain.v1.WalletService/GenerateBlocks",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("cusf.mainchain.v1.WalletService", "GenerateBlocks"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod wallet_service_server {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with WalletServiceServer.
    #[async_trait]
    pub trait WalletService: std::marker::Send + std::marker::Sync + 'static {
        async fn broadcast_withdrawal_bundle(
            &self,
            request: tonic::Request<super::BroadcastWithdrawalBundleRequest>,
        ) -> std::result::Result<
            tonic::Response<super::BroadcastWithdrawalBundleResponse>,
            tonic::Status,
        >;
        async fn create_bmm_critical_data_transaction(
            &self,
            request: tonic::Request<super::CreateBmmCriticalDataTransactionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateBmmCriticalDataTransactionResponse>,
            tonic::Status,
        >;
        async fn create_deposit_transaction(
            &self,
            request: tonic::Request<super::CreateDepositTransactionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateDepositTransactionResponse>,
            tonic::Status,
        >;
        async fn create_new_address(
            &self,
            request: tonic::Request<super::CreateNewAddressRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateNewAddressResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the CreateSidechainProposal method.
        type CreateSidechainProposalStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<
                    super::CreateSidechainProposalResponse,
                    tonic::Status,
                >,
            >
            + std::marker::Send
            + 'static;
        async fn create_sidechain_proposal(
            &self,
            request: tonic::Request<super::CreateSidechainProposalRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::CreateSidechainProposalStream>,
            tonic::Status,
        >;
        async fn create_wallet(
            &self,
            request: tonic::Request<super::CreateWalletRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateWalletResponse>,
            tonic::Status,
        >;
        async fn get_balance(
            &self,
            request: tonic::Request<super::GetBalanceRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBalanceResponse>,
            tonic::Status,
        >;
        async fn list_sidechain_deposit_transactions(
            &self,
            request: tonic::Request<super::ListSidechainDepositTransactionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListSidechainDepositTransactionsResponse>,
            tonic::Status,
        >;
        async fn list_transactions(
            &self,
            request: tonic::Request<super::ListTransactionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListTransactionsResponse>,
            tonic::Status,
        >;
        async fn list_unspent_outputs(
            &self,
            request: tonic::Request<super::ListUnspentOutputsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListUnspentOutputsResponse>,
            tonic::Status,
        >;
        async fn get_info(
            &self,
            request: tonic::Request<super::GetInfoRequest>,
        ) -> std::result::Result<tonic::Response<super::GetInfoResponse>, tonic::Status>;
        async fn send_transaction(
            &self,
            request: tonic::Request<super::SendTransactionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SendTransactionResponse>,
            tonic::Status,
        >;
        async fn unlock_wallet(
            &self,
            request: tonic::Request<super::UnlockWalletRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UnlockWalletResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the GenerateBlocks method.
        type GenerateBlocksStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::GenerateBlocksResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        async fn generate_blocks(
            &self,
            request: tonic::Request<super::GenerateBlocksRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::GenerateBlocksStream>,
            tonic::Status,
        >;
    }
    #[derive(Debug)]
    pub struct WalletServiceServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T> WalletServiceServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for WalletServiceServer<T>
    where
        T: WalletService,
        B: Body + std::marker::Send + 'static,
        B::Error: Into<StdError> + std::marker::Send + 'static,
    {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            match req.uri().path() {
                "/cusf.mainchain.v1.WalletService/BroadcastWithdrawalBundle" => {
                    #[allow(non_camel_case_types)]
                    struct BroadcastWithdrawalBundleSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<
                        super::BroadcastWithdrawalBundleRequest,
                    > for BroadcastWithdrawalBundleSvc<T> {
                        type Response = super::BroadcastWithdrawalBundleResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::BroadcastWithdrawalBundleRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::broadcast_withdrawal_bundle(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = BroadcastWithdrawalBundleSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/CreateBmmCriticalDataTransaction" => {
                    #[allow(non_camel_case_types)]
                    struct CreateBmmCriticalDataTransactionSvc<T: WalletService>(
                        pub Arc<T>,
                    );
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<
                        super::CreateBmmCriticalDataTransactionRequest,
                    > for CreateBmmCriticalDataTransactionSvc<T> {
                        type Response = super::CreateBmmCriticalDataTransactionResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::CreateBmmCriticalDataTransactionRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::create_bmm_critical_data_transaction(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateBmmCriticalDataTransactionSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/CreateDepositTransaction" => {
                    #[allow(non_camel_case_types)]
                    struct CreateDepositTransactionSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::CreateDepositTransactionRequest>
                    for CreateDepositTransactionSvc<T> {
                        type Response = super::CreateDepositTransactionResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::CreateDepositTransactionRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::create_deposit_transaction(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateDepositTransactionSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/CreateNewAddress" => {
                    #[allow(non_camel_case_types)]
                    struct CreateNewAddressSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::CreateNewAddressRequest>
                    for CreateNewAddressSvc<T> {
                        type Response = super::CreateNewAddressResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateNewAddressRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::create_new_address(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateNewAddressSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/CreateSidechainProposal" => {
                    #[allow(non_camel_case_types)]
                    struct CreateSidechainProposalSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::ServerStreamingService<
                        super::CreateSidechainProposalRequest,
                    > for CreateSidechainProposalSvc<T> {
                        type Response = super::CreateSidechainProposalResponse;
                        type ResponseStream = T::CreateSidechainProposalStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::CreateSidechainProposalRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::create_sidechain_proposal(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateSidechainProposalSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/CreateWallet" => {
                    #[allow(non_camel_case_types)]
                    struct CreateWalletSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::CreateWalletRequest>
                    for CreateWalletSvc<T> {
                        type Response = super::CreateWalletResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateWalletRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::create_wallet(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateWalletSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/GetBalance" => {
                    #[allow(non_camel_case_types)]
                    struct GetBalanceSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::GetBalanceRequest>
                    for GetBalanceSvc<T> {
                        type Response = super::GetBalanceResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetBalanceRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::get_balance(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetBalanceSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/ListSidechainDepositTransactions" => {
                    #[allow(non_camel_case_types)]
                    struct ListSidechainDepositTransactionsSvc<T: WalletService>(
                        pub Arc<T>,
                    );
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<
                        super::ListSidechainDepositTransactionsRequest,
                    > for ListSidechainDepositTransactionsSvc<T> {
                        type Response = super::ListSidechainDepositTransactionsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::ListSidechainDepositTransactionsRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::list_sidechain_deposit_transactions(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListSidechainDepositTransactionsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/ListTransactions" => {
                    #[allow(non_camel_case_types)]
                    struct ListTransactionsSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::ListTransactionsRequest>
                    for ListTransactionsSvc<T> {
                        type Response = super::ListTransactionsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListTransactionsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::list_transactions(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListTransactionsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/ListUnspentOutputs" => {
                    #[allow(non_camel_case_types)]
                    struct ListUnspentOutputsSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::ListUnspentOutputsRequest>
                    for ListUnspentOutputsSvc<T> {
                        type Response = super::ListUnspentOutputsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListUnspentOutputsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::list_unspent_outputs(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListUnspentOutputsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/GetInfo" => {
                    #[allow(non_camel_case_types)]
                    struct GetInfoSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::GetInfoRequest>
                    for GetInfoSvc<T> {
                        type Response = super::GetInfoResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetInfoRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::get_info(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetInfoSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/SendTransaction" => {
                    #[allow(non_camel_case_types)]
                    struct SendTransactionSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::SendTransactionRequest>
                    for SendTransactionSvc<T> {
                        type Response = super::SendTransactionResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SendTransactionRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::send_transaction(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = SendTransactionSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/UnlockWallet" => {
                    #[allow(non_camel_case_types)]
                    struct UnlockWalletSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::UnaryService<super::UnlockWalletRequest>
                    for UnlockWalletSvc<T> {
                        type Response = super::UnlockWalletResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::UnlockWalletRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::unlock_wallet(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = UnlockWalletSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cusf.mainchain.v1.WalletService/GenerateBlocks" => {
                    #[allow(non_camel_case_types)]
                    struct GenerateBlocksSvc<T: WalletService>(pub Arc<T>);
                    impl<
                        T: WalletService,
                    > tonic::server::ServerStreamingService<super::GenerateBlocksRequest>
                    for GenerateBlocksSvc<T> {
                        type Response = super::GenerateBlocksResponse;
                        type ResponseStream = T::GenerateBlocksStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GenerateBlocksRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WalletService>::generate_blocks(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GenerateBlocksSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        let mut response = http::Response::new(
                            tonic::body::Body::default(),
                        );
                        let headers = response.headers_mut();
                        headers
                            .insert(
                                tonic::Status::GRPC_STATUS,
                                (tonic::Code::Unimplemented as i32).into(),
                            );
                        headers
                            .insert(
                                http::header::CONTENT_TYPE,
                                tonic::metadata::GRPC_CONTENT_TYPE,
                            );
                        Ok(response)
                    })
                }
            }
        }
    }
    impl<T> Clone for WalletServiceServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    /// Generated gRPC service name
    pub const SERVICE_NAME: &str = "cusf.mainchain.v1.WalletService";
    impl<T> tonic::server::NamedService for WalletServiceServer<T> {
        const NAME: &'static str = SERVICE_NAME;
    }
}
