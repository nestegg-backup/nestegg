// The hub's web interface (decision 0034). The hub serves this page (`hub.html`)
// at `/`, together with the assets of the same build; it talks only to the hub
// API (`api/hub/v1/*`, see ../../../api/hub-openapi.yaml).
import { mount } from "svelte";
import Hub from "./Hub.svelte";
import { lang } from "../lib/locale";
import "../styles/tokens.css";
import "../styles/app.css";

document.documentElement.lang = lang;

mount(Hub, { target: document.getElementById("app")! });
