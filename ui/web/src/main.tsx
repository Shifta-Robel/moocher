import { Route, Router } from "@solidjs/router";
import { JSX, Show, lazy, onMount } from "solid-js";
import { render } from "solid-js/web";
import Layout from "./components/Layout";
import Login from "./pages/Login";
import Overview from "./pages/Overview";
import { connect, refreshAll, state } from "./store";
import "./index.css";

// Everything but the first screen loads on demand (small first bundle).
const Instances = lazy(() => import("./pages/Instances"));
const Instance = lazy(() => import("./pages/Instance"));
const Work = lazy(() => import("./pages/Work"));
const Questions = lazy(() => import("./pages/Questions"));
const Tasks = lazy(() => import("./pages/Tasks"));
const Quotas = lazy(() => import("./pages/Quotas"));
const Templates = lazy(() => import("./pages/Templates"));
const Nodes = lazy(() => import("./pages/Nodes"));
const Artifacts = lazy(() => import("./pages/Artifacts"));
const Notifications = lazy(() => import("./pages/Notifications"));
const Audit = lazy(() => import("./pages/Audit"));
const Settings = lazy(() => import("./pages/Settings"));

function App(p: { children?: JSX.Element }) {
  onMount(async () => {
    await refreshAll().catch(() => {});
    connect();
  });
  return (
    <Layout>
      <Show when={state.ready} fallback={<p class="text-sm text-slate-500">Loading…</p>}>
        {p.children}
      </Show>
    </Layout>
  );
}

if ("serviceWorker" in navigator && location.pathname !== "/login") {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

render(
  () => (
    <Router>
      <Route path="/login" component={Login} />
      <Route path="/" component={App}>
        <Route path="/" component={Overview} />
        <Route path="/instances" component={Instances} />
        <Route path="/instances/:id" component={Instance} />
        <Route path="/work" component={Work} />
        <Route path="/questions" component={Questions} />
        <Route path="/tasks" component={Tasks} />
        <Route path="/quotas" component={Quotas} />
        <Route path="/templates" component={Templates} />
        <Route path="/nodes" component={Nodes} />
        <Route path="/artifacts" component={Artifacts} />
        <Route path="/notifications" component={Notifications} />
        <Route path="/audit" component={Audit} />
        <Route path="/settings" component={Settings} />
        <Route path="*" component={() => <p>Not found.</p>} />
      </Route>
    </Router>
  ),
  document.getElementById("root")!,
);
