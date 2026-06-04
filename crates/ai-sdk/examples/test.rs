use std::borrow::Cow;

trait Provider {
    // some stuff
}

struct Message {
    // message all images, text, etc.
}

trait Context {
    type Ctx: Send + Sync;
    async fn load(&self, id: &str, ctx: Self::Ctx) -> Result<Cow<[Message]>, ()>;
    async fn save(&self, id: &str, ctx: Self::Ctx, messages: Cow<[Message]>) -> Result<(), ()>;
    async fn delete(&self, id: &str, ctx: Self::Ctx) -> Result<(), ()>;
    fn get_id(&self) -> &str;
}

trait AgentMiddleware {
    type Ctx: Send + Sync;
    // stuff we dicsused.
}

struct Agent {
    providers: Vec<Box<dyn Provider>>,
}

fn main() {
    let agent = Agent::new()
        .register_provider(OpenAiProvide::new())
        .register_provider(AnthropicProvider::new())
        .register_tools(vec![Tool1, Tool2, Tool3])
        .register_middleware(Middleware1)
        .register_middleware(Middleware2);
}
