// @generated
/// Generated client implementations.
pub mod validator_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct ValidatorClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl ValidatorClient<tonic::transport::Channel> {
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
    impl<T> ValidatorClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> ValidatorClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            ValidatorClient::new(InterceptedService::new(inner, interceptor))
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
        pub async fn connect_block(
            &mut self,
            request: impl tonic::IntoRequest<super::ConnectBlockRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ConnectBlockResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/ConnectBlock",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "ConnectBlock"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn disconnect_block(
            &mut self,
            request: impl tonic::IntoRequest<super::DisconnectBlockRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DisconnectBlockResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/DisconnectBlock",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "DisconnectBlock"));
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
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/GetCoinbasePSBT",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "GetCoinbasePSBT"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_deposits(
            &mut self,
            request: impl tonic::IntoRequest<super::GetDepositsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetDepositsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/GetDeposits",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "GetDeposits"));
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
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/GetSidechainProposals",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "GetSidechainProposals"));
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
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/GetSidechains",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "GetSidechains"));
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
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/GetCtip",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "GetCtip"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_main_block_height(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMainBlockHeightRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMainBlockHeightResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/GetMainBlockHeight",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "GetMainBlockHeight"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_main_chain_tip(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMainChainTipRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMainChainTipResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/validator.Validator/GetMainChainTip",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("validator.Validator", "GetMainChainTip"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod validator_server {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with ValidatorServer.
    #[async_trait]
    pub trait Validator: Send + Sync + 'static {
        async fn connect_block(
            &self,
            request: tonic::Request<super::ConnectBlockRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ConnectBlockResponse>,
            tonic::Status,
        >;
        async fn disconnect_block(
            &self,
            request: tonic::Request<super::DisconnectBlockRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DisconnectBlockResponse>,
            tonic::Status,
        >;
        async fn get_coinbase_psbt(
            &self,
            request: tonic::Request<super::GetCoinbasePsbtRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetCoinbasePsbtResponse>,
            tonic::Status,
        >;
        async fn get_deposits(
            &self,
            request: tonic::Request<super::GetDepositsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetDepositsResponse>,
            tonic::Status,
        >;
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
        async fn get_ctip(
            &self,
            request: tonic::Request<super::GetCtipRequest>,
        ) -> std::result::Result<tonic::Response<super::GetCtipResponse>, tonic::Status>;
        async fn get_main_block_height(
            &self,
            request: tonic::Request<super::GetMainBlockHeightRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMainBlockHeightResponse>,
            tonic::Status,
        >;
        async fn get_main_chain_tip(
            &self,
            request: tonic::Request<super::GetMainChainTipRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMainChainTipResponse>,
            tonic::Status,
        >;
    }
    #[derive(Debug)]
    pub struct ValidatorServer<T: Validator> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T: Validator> ValidatorServer<T> {
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
    impl<T, B> tonic::codegen::Service<http::Request<B>> for ValidatorServer<T>
    where
        T: Validator,
        B: Body + Send + 'static,
        B::Error: Into<StdError> + Send + 'static,
    {
        type Response = http::Response<tonic::body::BoxBody>;
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
                "/validator.Validator/ConnectBlock" => {
                    #[allow(non_camel_case_types)]
                    struct ConnectBlockSvc<T: Validator>(pub Arc<T>);
                    impl<
                        T: Validator,
                    > tonic::server::UnaryService<super::ConnectBlockRequest>
                    for ConnectBlockSvc<T> {
                        type Response = super::ConnectBlockResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ConnectBlockRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Validator>::connect_block(&inner, request).await
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
                        let method = ConnectBlockSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/validator.Validator/DisconnectBlock" => {
                    #[allow(non_camel_case_types)]
                    struct DisconnectBlockSvc<T: Validator>(pub Arc<T>);
                    impl<
                        T: Validator,
                    > tonic::server::UnaryService<super::DisconnectBlockRequest>
                    for DisconnectBlockSvc<T> {
                        type Response = super::DisconnectBlockResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DisconnectBlockRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Validator>::disconnect_block(&inner, request).await
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
                        let method = DisconnectBlockSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/validator.Validator/GetCoinbasePSBT" => {
                    #[allow(non_camel_case_types)]
                    struct GetCoinbasePSBTSvc<T: Validator>(pub Arc<T>);
                    impl<
                        T: Validator,
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
                                <T as Validator>::get_coinbase_psbt(&inner, request).await
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
                        let codec = tonic::codec::ProstCodec::default();
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
                "/validator.Validator/GetDeposits" => {
                    #[allow(non_camel_case_types)]
                    struct GetDepositsSvc<T: Validator>(pub Arc<T>);
                    impl<
                        T: Validator,
                    > tonic::server::UnaryService<super::GetDepositsRequest>
                    for GetDepositsSvc<T> {
                        type Response = super::GetDepositsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetDepositsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Validator>::get_deposits(&inner, request).await
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
                        let method = GetDepositsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/validator.Validator/GetSidechainProposals" => {
                    #[allow(non_camel_case_types)]
                    struct GetSidechainProposalsSvc<T: Validator>(pub Arc<T>);
                    impl<
                        T: Validator,
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
                                <T as Validator>::get_sidechain_proposals(&inner, request)
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
                        let codec = tonic::codec::ProstCodec::default();
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
                "/validator.Validator/GetSidechains" => {
                    #[allow(non_camel_case_types)]
                    struct GetSidechainsSvc<T: Validator>(pub Arc<T>);
                    impl<
                        T: Validator,
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
                                <T as Validator>::get_sidechains(&inner, request).await
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
                        let codec = tonic::codec::ProstCodec::default();
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
                "/validator.Validator/GetCtip" => {
                    #[allow(non_camel_case_types)]
                    struct GetCtipSvc<T: Validator>(pub Arc<T>);
                    impl<T: Validator> tonic::server::UnaryService<super::GetCtipRequest>
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
                                <T as Validator>::get_ctip(&inner, request).await
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
                        let codec = tonic::codec::ProstCodec::default();
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
                "/validator.Validator/GetMainBlockHeight" => {
                    #[allow(non_camel_case_types)]
                    struct GetMainBlockHeightSvc<T: Validator>(pub Arc<T>);
                    impl<
                        T: Validator,
                    > tonic::server::UnaryService<super::GetMainBlockHeightRequest>
                    for GetMainBlockHeightSvc<T> {
                        type Response = super::GetMainBlockHeightResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetMainBlockHeightRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Validator>::get_main_block_height(&inner, request)
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
                        let method = GetMainBlockHeightSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/validator.Validator/GetMainChainTip" => {
                    #[allow(non_camel_case_types)]
                    struct GetMainChainTipSvc<T: Validator>(pub Arc<T>);
                    impl<
                        T: Validator,
                    > tonic::server::UnaryService<super::GetMainChainTipRequest>
                    for GetMainChainTipSvc<T> {
                        type Response = super::GetMainChainTipResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetMainChainTipRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Validator>::get_main_chain_tip(&inner, request).await
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
                        let method = GetMainChainTipSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                        Ok(
                            http::Response::builder()
                                .status(200)
                                .header("grpc-status", tonic::Code::Unimplemented as i32)
                                .header(
                                    http::header::CONTENT_TYPE,
                                    tonic::metadata::GRPC_CONTENT_TYPE,
                                )
                                .body(empty_body())
                                .unwrap(),
                        )
                    })
                }
            }
        }
    }
    impl<T: Validator> Clone for ValidatorServer<T> {
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
    impl<T: Validator> tonic::server::NamedService for ValidatorServer<T> {
        const NAME: &'static str = "validator.Validator";
    }
}
