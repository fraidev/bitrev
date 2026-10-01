"use strict";

const form = document.getElementById("login-form");
const error = document.getElementById("login-error");

if (new URLSearchParams(location.search).get("error")) {
  error.hidden = false;
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  error.hidden = true;
  const response = await fetch("/api/v1/login", {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams(new FormData(form)),
    credentials: "same-origin",
  });
  const landed = new URL(response.url, location.origin);
  if (response.ok && landed.pathname === "/" && !landed.searchParams.has("error")) {
    location.assign("/");
    return;
  }
  error.hidden = false;
});
