import { mount } from "svelte";
import App from "./App.svelte";
import { lang } from "./lib/locale";
import "./styles/tokens.css";
import "./styles/app.css";

document.documentElement.lang = lang;

mount(App, { target: document.getElementById("app")! });
