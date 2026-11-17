@browser
Feature: htmx edge cases the server-bytes layers cannot observe
  These @browser scenarios drive a test-only /fixtures/* route set (compiled only
  under the `test-fixtures` cargo feature — it ships nothing) to prove the browser
  harness observes htmx behaviors P1 (DOM of the server bytes) and P2 (byte
  snapshots) cannot: an out-of-band swap landing in a *second* region, a
  response-header retarget moving a byte-identical body elsewhere, and a
  response-header push-url changing the browser's location. Like the rest of the
  browser layer they are advisory and run behind UI_BROWSER_TESTS=1.

  # hx-swap-oob: one response updates TWO regions — the hx-target and an
  # out-of-band sibling. The OOB element is in the response bytes regardless; only
  # the browser proves a SECOND region actually updated.
  Scenario: An out-of-band swap updates a second region
    Given the ui-htmx BFF is running
    When I load the "/fixtures/oob" fixture in a browser
    And I click the fixture button
    Then the "#main-region" region shows "swapped main"
    And the "#toast-region" region shows "swapped toast"

  # Negative: the same OOB response, but the page has no matching target, so htmx
  # drops the OOB content. The response bytes are identical to the positive case;
  # only the DOM outcome differs — a divergence P1/P2 cannot see.
  Scenario: An out-of-band swap with no matching target is dropped
    Given the ui-htmx BFF is running
    When I load the "/fixtures/oob-missing" fixture in a browser
    And I click the fixture button
    Then the "#main-region" region shows "swapped main"
    And the text "swapped toast" appears nowhere in the page

  # HX-Retarget + HX-Reswap: the response body is a plain fragment (what a normal
  # swap carries), but response headers move the swap to #secondary and re-swap it
  # as innerHTML (the page declares outerHTML). P2 would compare the body and see
  # nothing; the divergence lives in the headers (a Layer A tripwire) and the
  # landing is only browser-observable.
  Scenario: A response-header retarget moves a byte-identical body elsewhere
    Given the ui-htmx BFF is running
    When I load the "/fixtures/retarget" fixture in a browser
    And I click the fixture button
    Then the "#secondary" region shows "retargeted content"
    And the "#primary" region still shows "initial primary"
    And the "/fixtures/retarget/swap" response carries "HX-Retarget" of "#secondary" with body "retargeted content"

  # HX-Push-Url: a response header changes the browser URL/history without a
  # navigation — observable only in the browser.
  Scenario: A response-header push-url changes the browser location
    Given the ui-htmx BFF is running
    When I load the "/fixtures/push-url" fixture in a browser
    And I click the fixture button
    Then the "#main-region" region shows "pushed main"
    And the browser URL contains "/fixtures/pushed"
