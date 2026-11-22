const LLM_HUB_PROVIDER_ID = "llm-hub";
const SESSION_HEADER = "x-llm-hub-session-id";

interface HttpRequestEvent {
  readonly sessionID: string;
  readonly request: { readonly headers: Headers };
}

interface PluginContext {
  readonly session: {
    hook(
      name: "http.request",
      callback: (event: HttpRequestEvent) => void,
      options: { providerID: string },
    ): Promise<{ dispose(): Promise<void> }>;
  };
}

export default {
  id: "llm-hub-session",
  async setup(ctx: PluginContext) {
    const registration = await ctx.session.hook(
      "http.request",
      (event) => {
        event.request.headers.set(SESSION_HEADER, event.sessionID);
      },
      { providerID: LLM_HUB_PROVIDER_ID },
    );
    return () => registration.dispose();
  },
};
