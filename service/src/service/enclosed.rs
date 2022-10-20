use core::future::Future;

use crate::pipeline::{marker::BuildEnclosed, PipelineE, PipelineT};

use super::Service;

impl<F, Arg, T> Service<Arg> for PipelineT<F, T, BuildEnclosed>
where
    F: Service<Arg>,
    T: Service<F::Response>,
{
    type Response = T::Response;
    type Error = PipelineE<F::Error, T::Error>;
    type Future<'f> = impl Future<Output = Result<Self::Response, Self::Error>> where Self: 'f;

    fn call(&self, arg: Arg) -> Self::Future<'_> {
        async {
            let service = self.first.call(arg).await.map_err(PipelineE::First)?;
            self.second.call(service).await.map_err(PipelineE::Second)
        }
    }
}
