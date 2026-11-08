import { mount } from "svelte";
import App from "./App.svelte";
import { lang } from "./lib/locale";
import { initBrowserSession } from "./lib/session";
import "./styles/tokens.css";
import "./styles/app.css";

// Take the session token from `#token=…` before anything talks to the API.
initBrowserSession();
document.documentElement.lang = lang;

mount(App, { target: document.getElementById("app")! });
