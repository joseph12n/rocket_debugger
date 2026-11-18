    "use strict";

    /*
     * Rutas del servidor local. Son rutas HTTP, no rutas del sistema de ficheros:
     * el contrato las documenta como `GET /api/review` y `GET /api/pack`, asi que
     * aqui no se adivina un prefijo ni se compone una ruta de disco.
     */
    const API_REVIEW = "/api/review";
    const API_PACK = "/api/pack";

    /*
     * Enlace profundo: `?robot=<nombre>&db=<etiqueta>` abre el panel de ese
     * robot. Es el mismo par que viaja a `/api/pack`, asi que la URL se puede
     * copiar y mandar a otro sin decirle que busque la fila y pulse.
     */
    const LINK_ROBOT = "robot";
    const LINK_DB = "db";

    /*
     * Umbral de "rojo" para hallazgos accionables. El barrido real del proyecto
     * mide ~2,7 accionables por robot afectado, asi que 5 es el doble de la media:
     * a partir de ahi el robot no es "uno mas" y merece rojo en vez de ambar.
     */
    const ACTIONABLE_HOT = 5;

    const NUMBER_FORMAT = new Intl.NumberFormat("es-ES", { maximumFractionDigits: 0 });
    const TIME_FORMAT = new Intl.DateTimeFormat("es-ES", {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
    const DATE_TIME_FORMAT = new Intl.DateTimeFormat("es-ES", {
      dateStyle: "short",
      timeStyle: "medium",
    });

    const COUNT_LABELS = {
      nodes: ["nodo", "nodos"],
      actionable: ["hallazgo accionable", "hallazgos accionables"],
      hygiene: ["hallazgo de higiene", "hallazgos de higiene"],
      live: ["error vivo", "errores vivos"],
    };

    const byId = (id) => document.getElementById(id);

    const sourceChip = byId("source");
    const dbCountChip = byId("db-count");
    const updatedChip = byId("updated-at");
    const refreshButton = byId("refresh");
    const totalRobots = byId("total-robots");
    const totalActionable = byId("total-actionable");
    const totalHygiene = byId("total-hygiene");
    const totalLive = byId("total-live");
    const board = byId("board");
    const filterInput = byId("filter");
    const clearFilterButton = byId("clear-filter");
    const visibleCount = byId("visible-count");
    const viewsContainer = byId("views");
    const tableWrap = byId("table-wrap");
    const table = byId("tabla-robots");
    const rowsBody = byId("rows");
    const rowTemplate = byId("row-template");
    const liveRegion = byId("live");
    const errorMessage = byId("view-error-message");
    const retryButton = byId("retry");
    const emptyRefreshButton = byId("empty-refresh");
    const filteredMessage = byId("view-filtered-message");
    const filteredClearButton = byId("filtered-clear");
    const panel = byId("panel");
    const panelTitle = byId("panel-title");
    const panelMeta = byId("panel-meta");
    const panelCloseButton = byId("panel-close");
    const panelBody = byId("panel-body");
    const panelLoading = byId("panel-loading");
    const panelError = byId("panel-error");
    const panelErrorMessage = byId("panel-error-message");
    const panelRetryButton = byId("panel-retry");
    const panelContentWrap = byId("panel-content-wrap");
    const panelSize = byId("panel-size");
    const panelContent = byId("panel-content");
    const scrim = byId("scrim");

    const views = {
      loading: byId("view-loading"),
      error: byId("view-error"),
      empty: byId("view-empty"),
      filtered: byId("view-filtered"),
    };

    const sortButtons = Array.from(table.querySelectorAll("[data-sort]"));

    const state = {
      rows: [],
      loaded: false,
      filter: "",
      sort: { key: "", direction: 1 },
      reviewController: null,
      packController: null,
      panel: { robot: "", db: "", opener: null },
    };

    /* ------------------------------------------------------------------ */
    /* Utilidades                                                          */
    /* ------------------------------------------------------------------ */

    function asCount(value) {
      return Number.isFinite(value) ? value : null;
    }

    function formatCount(value) {
      return value === null ? "—" : NUMBER_FORMAT.format(value);
    }

    function plural(value, one, many) {
      return value === 1 ? one : many;
    }

    function robotsLabel(count) {
      return count === 1 ? "1 robot" : `${NUMBER_FORMAT.format(count)} robots`;
    }

    function dbLabel(value) {
      if (value === null) return "—";
      return value === 1 ? "1 db" : `${NUMBER_FORMAT.format(value)} dbs`;
    }

    /* El filtro ignora mayusculas y tildes: "gmail" tiene que encontrar
       "ML_07_Gmail" y "Giménez" tiene que encontrar "Gimenez". */
    function normalise(text) {
      return String(text ?? "")
        .normalize("NFD")
        .replace(/[\u0300-\u036f]/g, "")
        .toLowerCase();
    }

    function messageOf(error) {
      if (error instanceof TypeError) {
        return "No pude hablar con el servidor local. Comprueba que siga en marcha y vuelve a intentarlo.";
      }
      if (error instanceof Error && error.message) {
        return error.message;
      }
      return String(error);
    }

    function announce(text) {
      liveRegion.textContent = text;
    }

    /*
     * El mensaje de error lo escribe el servidor y se pinta como texto. Se
     * prefiere el campo `error` del JSON porque es el que da el motivo real
     * ("ruta desconocida: /api/review"), no el codigo HTTP; si el cuerpo no es
     * JSON se muestra tal cual, recortado para no volcar una pagina entera.
     */
    async function serverMessage(response, fallback) {
      let text;
      try {
        text = await response.text();
      } catch (error) {
        return fallback;
      }
      const trimmed = text.trim();
      if (!trimmed) return fallback;
      try {
        const payload = JSON.parse(trimmed);
        if (payload && typeof payload.error === "string" && payload.error.trim()) {
          return payload.error.trim();
        }
      } catch (error) {
        /* el cuerpo no es JSON: se usa como esta */
      }
      return trimmed.length > 400 ? `${trimmed.slice(0, 400)}…` : trimmed;
    }

    /* ------------------------------------------------------------------ */
    /* Estados de la bandeja                                               */
    /* ------------------------------------------------------------------ */

    function showView(name) {
      if (name === "table") {
        viewsContainer.hidden = true;
        tableWrap.hidden = false;
        return;
      }
      viewsContainer.hidden = false;
      tableWrap.hidden = true;
      for (const [key, element] of Object.entries(views)) {
        element.hidden = key !== name;
      }
    }

    function setRefreshBusy(busy) {
      refreshButton.disabled = busy;
      refreshButton.classList.toggle("is-busy", busy);
      refreshButton.title = busy ? "Leyendo la bandeja…" : "Volver a leer la bandeja";
      board.setAttribute("aria-busy", busy ? "true" : "false");
    }

    function setTotals(totals) {
      const value = (key) => {
        if (!totals || typeof totals !== "object") return "—";
        return formatCount(asCount(totals[key]));
      };
      totalRobots.textContent = value("robots");
      totalActionable.textContent = value("accionables");
      totalHygiene.textContent = value("higiene");
      totalLive.textContent = value("vivos");
    }

    function resetHeader() {
      sourceChip.textContent = "—";
      dbCountChip.textContent = "—";
      updatedChip.textContent = "sin leer";
      updatedChip.removeAttribute("title");
      setTotals(null);
    }

    function showError(message) {
      state.loaded = false;
      state.rows = [];
      resetHeader();
      visibleCount.textContent = "—";
      errorMessage.textContent = message;
      showView("error");
      announce("No se pudo cargar la bandeja.");
    }

    /* ------------------------------------------------------------------ */
    /* Render                                                              */
    /* ------------------------------------------------------------------ */

    function rowSeverity(row) {
      if ((row.live ?? 0) > 0) return "row--alert";
      if ((row.actionable ?? 0) > 0) return "row--work";
      if ((row.hygiene ?? 0) > 0) return "row--calm";
      return "row--idle";
    }

    function paintCount(cell, value, kind) {
      const badge = cell.querySelector(".value");
      badge.className = "value";
      if (value === null) {
        badge.textContent = "—";
        badge.classList.add("value--unknown");
        cell.title = "sin dato";
        return;
      }
      const [one, many] = COUNT_LABELS[kind];
      badge.textContent = formatCount(value);
      cell.title = `${formatCount(value)} ${plural(value, one, many)}`;
      if (value === 0) {
        badge.classList.add("value--zero");
      } else if (kind === "actionable") {
        badge.classList.add(value >= ACTIONABLE_HOT ? "value--alert" : "value--warn");
      } else if (kind === "hygiene") {
        badge.classList.add("value--calm");
      } else if (kind === "live") {
        badge.classList.add("value--alert");
      }
    }

    function renderRows(rows) {
      const fragment = document.createDocumentFragment();
      for (const row of rows) {
        const node = rowTemplate.content.firstElementChild.cloneNode(true);
        node.dataset.index = String(row.index);
        node.classList.add(rowSeverity(row));

        const link = node.querySelector(".robot-link");
        link.textContent = row.robot || "(sin nombre)";
        const label = row.robot
          ? `Abrir el context pack de ${row.robot}`
          : "Abrir el context pack de un robot sin nombre";
        link.setAttribute("aria-label", label);
        link.title = label;

        const db = node.querySelector(".db");
        db.textContent = row.db || "—";
        db.title = row.db || "";

        paintCount(node.querySelector('[data-col="nodes"]'), row.nodes, "nodes");
        paintCount(node.querySelector('[data-col="actionable"]'), row.actionable, "actionable");
        paintCount(node.querySelector('[data-col="hygiene"]'), row.hygiene, "hygiene");
        paintCount(node.querySelector('[data-col="live"]'), row.live, "live");

        fragment.append(node);
      }
      rowsBody.replaceChildren(fragment);
    }

    /*
     * Orden por defecto: el del servidor. La pagina solo reordena cuando el
     * usuario pulsa una cabecera, y ese orden se mantiene al filtrar.
     */
    function sortRows(rows) {
      if (!state.sort.key) return rows;
      const key = state.sort.key;
      const direction = state.sort.direction;
      const number = (value) => (Number.isFinite(value) ? value : -1);

      return rows.slice().sort((left, right) => {
        const a = left[key];
        const b = right[key];
        let order;
        if (typeof a === "number" || typeof b === "number") {
          order = number(a) - number(b);
        } else {
          order = String(a ?? "").localeCompare(String(b ?? ""), "es", {
            numeric: true,
            sensitivity: "base",
          });
        }
        if (order === 0) {
          order =
            left.robot.localeCompare(right.robot, "es", { sensitivity: "base" }) ||
            left.index - right.index;
        }
        return order * direction;
      });
    }

    function updateSortIndicators() {
      for (const button of sortButtons) {
        const key = button.dataset.sort;
        const header = button.closest("th");
        const active = state.sort.key === key;
        button.querySelector(".th-arrow").textContent = active
          ? state.sort.direction > 0
            ? "▲"
            : "▼"
          : "";
        header.setAttribute(
          "aria-sort",
          active ? (state.sort.direction > 0 ? "ascending" : "descending") : "none"
        );
      }
    }

    function updateVisibleCount(visible) {
      const total = state.rows.length;
      visibleCount.textContent = state.filter
        ? `${NUMBER_FORMAT.format(visible)} de ${robotsLabel(total)}`
        : robotsLabel(total);
    }

    function render() {
      if (!state.loaded) return;

      const needle = normalise(state.filter);
      const filtered = needle
        ? state.rows.filter((row) => normalise(row.robot).includes(needle))
        : state.rows.slice();

      updateSortIndicators();
      updateVisibleCount(filtered.length);

      if (state.rows.length === 0) {
        showView("empty");
        return;
      }

      if (filtered.length === 0) {
        filteredMessage.textContent = `Ningún robot coincide con «${state.filter}».`;
        showView("filtered");
        return;
      }

      renderRows(sortRows(filtered));
      showView("table");
    }

    /* ------------------------------------------------------------------ */
    /* Carga de la bandeja                                                 */
    /* ------------------------------------------------------------------ */

    /*
     * Valida la forma del contrato antes de tocar el DOM. Un JSON valido pero
     * sin `rows`/`totals` es un fallo del servidor y se ensena como error, no
     * como una bandeja vacia que pareceria "todo limpio".
     */
    function parseReview(payload) {
      if (
        !payload ||
        typeof payload !== "object" ||
        !Array.isArray(payload.rows) ||
        !payload.totals ||
        typeof payload.totals !== "object"
      ) {
        throw new Error("El servidor devolvió un JSON sin `rows` y `totals`.");
      }

      const rows = payload.rows.map((raw, index) => {
        const row = raw && typeof raw === "object" ? raw : {};
        return {
          index,
          robot: typeof row.robot === "string" ? row.robot : "",
          db: typeof row.db === "string" ? row.db : "",
          nodes: asCount(row.nodes),
          actionable: asCount(row.accionables),
          hygiene: asCount(row.higiene),
          live: asCount(row.vivos),
        };
      });

      return {
        source: typeof payload.source === "string" ? payload.source : "—",
        dbs: asCount(payload.dbs),
        rows,
        totals: payload.totals,
      };
    }

    async function loadReview() {
      state.reviewController?.abort();
      const controller = new AbortController();
      state.reviewController = controller;

      state.loaded = false;
      setRefreshBusy(true);
      showView("loading");

      try {
        const response = await fetch(API_REVIEW, {
          cache: "no-store",
          signal: controller.signal,
          headers: { Accept: "application/json" },
        });

        if (!response.ok) {
          throw new Error(
            await serverMessage(response, `La bandeja respondió HTTP ${response.status}.`)
          );
        }

        let payload;
        try {
          payload = await response.json();
        } catch (error) {
          throw new Error("El servidor respondió algo que no es JSON.");
        }

        const review = parseReview(payload);

        state.rows = review.rows;
        state.loaded = true;

        sourceChip.textContent = review.source;
        sourceChip.title = review.source;
        dbCountChip.textContent = dbLabel(review.dbs);
        const now = new Date();
        updatedChip.textContent = `actualizado ${TIME_FORMAT.format(now)}`;
        updatedChip.title = DATE_TIME_FORMAT.format(now);
        setTotals(review.totals);

        render();
        announce(`${robotsLabel(state.rows.length)} en la bandeja.`);
      } catch (error) {
        // Abortar es lo que hace un refresco nuevo con la peticion anterior: no
        // es un fallo que merezca un estado de error.
        if (error.name === "AbortError") return;
        showError(messageOf(error));
      } finally {
        if (state.reviewController === controller) {
          state.reviewController = null;
          setRefreshBusy(false);
        }
      }

      // El enlace profundo espera a tener filas pintadas, asi que va aqui y no
      // dentro del `try`: abrir el panel no es parte de cargar la bandeja, y su
      // fallo no deberia pintarse como un fallo de la bandeja.
      applyUrlLink();
    }

    /* ------------------------------------------------------------------ */
    /* Enlace profundo                                                     */
    /* ------------------------------------------------------------------ */

    /*
     * La URL es el estado del panel: si trae `robot`, ese robot esta abierto.
     * Se lee con `URLSearchParams` porque decodifica `%5C` y `%20` por nosotros:
     * la etiqueta de la db es una ruta relativa (`BBRE2\robot_run.db`) y el
     * nombre del robot puede llevar espacios, acentos o un `&`.
     */
    function readLink() {
      const params = new URLSearchParams(location.search);
      return {
        robot: params.get(LINK_ROBOT) ?? "",
        db: params.get(LINK_DB) ?? "",
      };
    }

    /*
     * Escribe el enlace sin recargar y sin apilar historial: cerrar el panel no
     * puede costar un "atras". Un robot sin nombre no da un enlace util, asi que
     * su caso es el de "ningun panel": se quitan los parametros en vez de dejar
     * los del robot anterior mintiendo en la barra de direcciones.
     */
    function writeLink(link) {
      const url = new URL(location.href);
      if (link && link.robot) {
        url.searchParams.set(LINK_ROBOT, link.robot);
        if (link.db) url.searchParams.set(LINK_DB, link.db);
        else url.searchParams.delete(LINK_DB);
      } else {
        url.searchParams.delete(LINK_ROBOT);
        url.searchParams.delete(LINK_DB);
      }

      const before = `${location.pathname}${location.search}${location.hash}`;
      if (`${url.pathname}${url.search}${url.hash}` === before) return;

      try {
        history.replaceState(history.state, "", url.href);
      } catch (error) {
        // Safari limita `replaceState` a unas pocas llamadas seguidas: quedarse
        // sin enlace molesta, pero romper el clic no es una opcion.
      }
    }

    /*
     * Fila a la que apunta un enlace. El nombre tiene que coincidir entero; la
     * db solo desempata entre homonimos (el mismo robot en dos bandejas). Si el
     * robot esta pero en otra db se abre igual: el panel ensena la db real y esa
     * pasa a ser la del enlace, que es lo util cuando la db se ha movido.
     */
    function rowForLink(link) {
      if (!link.robot) return null;
      const sameName = state.rows.filter((row) => row.robot === link.robot);
      if (sameName.length === 0) return null;
      if (link.db) {
        const sameDb = sameName.find((row) => row.db === link.db);
        if (sameDb) return sameDb;
      }
      return sameName[0];
    }

    /*
     * Pone el panel donde dice la URL. Se llama al terminar de cargar la bandeja
     * (hacen falta las filas) y en `popstate` (no hace falta volver a pedirlas).
     * Si ya esta abierto ese robot no se toca nada: un "atras" que vuelve al
     * mismo sitio no debe recargar el context pack.
     */
    function applyUrlLink() {
      if (!state.loaded) return;

      const link = readLink();
      if (!link.robot) {
        closePanel({ restoreFocus: false, updateUrl: false });
        return;
      }

      const isOpen = state.panel.robot === link.robot && (!link.db || state.panel.db === link.db);
      if (!panel.hidden && isOpen) return;

      const row = rowForLink(link);
      if (!row) {
        openMissing(link);
        return;
      }
      openPanel(row, rowLinkElement(row));
    }

    /*
     * El foco vuelve a la fila al cerrar, pero con un filtro puesto puede no
     * estar pintada: en ese caso el panel se abre igual y al cerrar no se mueve
     * el foco a ningun sitio raro.
     */
    function rowLinkElement(row) {
      return rowsBody.querySelector(`tr[data-index="${row.index}"] .robot-link`);
    }

    /* ------------------------------------------------------------------ */
    /* Panel del context pack                                              */
    /* ------------------------------------------------------------------ */

    function panelMetaFor(row) {
      return [
        row.db || "db desconocida",
        `${formatCount(row.nodes)} nodos`,
        `${formatCount(row.actionable)} accionables`,
        `${formatCount(row.hygiene)} higiene`,
        `${formatCount(row.live)} vivos`,
      ].join(" · ");
    }

    function setPanelView(name) {
      panelLoading.hidden = name !== "loading";
      panelError.hidden = name !== "error";
      panelContentWrap.hidden = name !== "content";
      panelBody.setAttribute("aria-busy", name === "loading" ? "true" : "false");
    }

    /*
     * Ensenar el panel con la cabecera ya puesta; el estado del cuerpo (cargando,
     * error o contenido) lo decide quien llama. Aqui se limpia el pack anterior:
     * al saltar de un robot a otro por la URL no puede quedar ni un instante el
     * contenido del robot equivocado en el DOM.
     */
    function mountPanel({ robot, db, title, meta, opener }) {
      state.panel.robot = robot;
      state.panel.db = db;
      state.panel.opener = opener instanceof HTMLElement ? opener : document.activeElement;
      panelTitle.textContent = title;
      panelMeta.textContent = meta;
      panelContent.textContent = "";
      panelSize.textContent = "";
      panelErrorMessage.textContent = "";
      panel.hidden = false;
      scrim.hidden = false;
      document.body.classList.add("panel-open");
      panelCloseButton.focus();
    }

    function openPanel(row, opener) {
      mountPanel({
        robot: row.robot,
        db: row.db || "",
        title: row.robot || "(robot sin nombre)",
        meta: panelMetaFor(row),
        opener,
      });
      setPanelView("loading");
      // El enlace se escribe antes de pedir el pack: si la peticion tarda o
      // falla, copiar la barra de direcciones sigue llevando a este robot.
      writeLink({ robot: row.robot, db: row.db || "" });
      void loadPack();
    }

    /*
     * Un enlace puede apuntar a un robot que ya no esta en la bandeja: se
     * renombro, se borro o el servidor esta mirando otras dbs. Se abre el panel
     * con el motivo; dejar la pagina como si no se hubiera pedido nada hace
     * pensar que el enlace se ignoro.
     */
    function openMissing(link) {
      mountPanel({
        robot: link.robot,
        db: link.db,
        title: link.robot,
        meta: [link.db || "db desconocida", "no está en la bandeja"].join(" · "),
        opener: null,
      });
      panelErrorMessage.textContent = `No encontré "${link.robot}" en esta bandeja.`;
      setPanelView("error");
      writeLink(link);
      announce(`No encontré "${link.robot}" en esta bandeja.`);
    }

    function closePanel({ restoreFocus = true, updateUrl = true } = {}) {
      if (panel.hidden) return;
      state.packController?.abort();
      state.packController = null;
      // Cerrar deja la URL sin enlace: el enlace describe lo que se ve, y lo que
      // se ve es la bandeja entera.
      if (updateUrl) writeLink(null);
      panel.hidden = true;
      scrim.hidden = true;
      document.body.classList.remove("panel-open");
      // No dejar en el DOM el pack del robot anterior: si se reabre otro, no
      // puede aparecer ni un instante el contenido equivocado.
      panelContent.textContent = "";
      panelSize.textContent = "";
      panelErrorMessage.textContent = "";
      const opener = state.panel.opener;
      state.panel.opener = null;
      state.panel.robot = "";
      state.panel.db = "";
      if (restoreFocus && opener && document.contains(opener)) {
        opener.focus();
      }
    }

    async function loadPack() {
      state.packController?.abort();
      const controller = new AbortController();
      state.packController = controller;
      const robot = state.panel.robot;
      const db = state.panel.db;
      setPanelView("loading");

      try {
        const response = await fetch(`${API_PACK}?robot=${encodeURIComponent(robot)}${db ? `&db=${encodeURIComponent(db)}` : ""}`, {
          cache: "no-store",
          signal: controller.signal,
          headers: { Accept: "text/plain" },
        });

        if (!response.ok) {
          throw new Error(
            await serverMessage(response, `El context pack respondió HTTP ${response.status}.`)
          );
        }

        const text = await response.text();
        if (controller.signal.aborted || state.panel.robot !== robot) return;
        if (!text.trim()) {
          throw new Error("El servidor devolvió un context pack vacío.");
        }

        panelContent.textContent = text;
        panelSize.textContent = `${NUMBER_FORMAT.format(text.length)} caracteres · markdown`;
        setPanelView("content");
      } catch (error) {
        if (error.name === "AbortError") return;
        if (state.panel.robot !== robot) return;
        panelErrorMessage.textContent = messageOf(error);
        setPanelView("error");
      } finally {
        if (state.packController === controller) {
          state.packController = null;
        }
      }
    }

    function focusableIn(container) {
      return Array.from(
        container.querySelectorAll(
          'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'
        )
      ).filter((element) => element.getClientRects().length > 0);
    }

    /* ------------------------------------------------------------------ */
    /* Eventos                                                             */
    /* ------------------------------------------------------------------ */

    refreshButton.addEventListener("click", () => {
      closePanel({ restoreFocus: false });
      void loadReview();
    });

    retryButton.addEventListener("click", () => void loadReview());
    emptyRefreshButton.addEventListener("click", () => void loadReview());
    filteredClearButton.addEventListener("click", () => clearFilter());
    clearFilterButton.addEventListener("click", () => clearFilter());

    filterInput.addEventListener("input", () => {
      state.filter = filterInput.value.trim();
      clearFilterButton.hidden = filterInput.value === "";
      render();
    });

    filterInput.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && filterInput.value !== "") {
        event.preventDefault();
        clearFilter();
      }
    });

    function clearFilter() {
      filterInput.value = "";
      state.filter = "";
      clearFilterButton.hidden = true;
      filterInput.focus();
      render();
    }

    table.querySelector("thead").addEventListener("click", (event) => {
      const button = event.target.closest("[data-sort]");
      if (!button) return;
      const key = button.dataset.sort;
      const numeric = key === "nodes" || key === "actionable" || key === "hygiene" || key === "live";

      if (state.sort.key === key) {
        state.sort.direction *= -1;
      } else {
        state.sort.key = key;
        // Los numeros se ordenan de mayor a menor por defecto: lo que tiene mas
        // trabajo es lo que se quiere ver arriba.
        state.sort.direction = numeric ? -1 : 1;
      }
      render();
    });

    rowsBody.addEventListener("click", (event) => {
      if (!(event.target instanceof Element)) return;
      const rowElement = event.target.closest("tr[data-index]");
      if (!rowElement) return;
      const row = state.rows[Number(rowElement.dataset.index)];
      if (!row) return;
      openPanel(row, rowElement.querySelector(".robot-link"));
    });

    panelCloseButton.addEventListener("click", () => closePanel());
    scrim.addEventListener("click", () => closePanel());
    panelRetryButton.addEventListener("click", () => void loadPack());

    document.addEventListener("keydown", (event) => {
      if (panel.hidden) return;

      if (event.key === "Escape") {
        event.preventDefault();
        closePanel();
        return;
      }

      if (event.key !== "Tab") return;

      // El panel es modal: sin trampa de foco, tabular se iria a la tabla del
      // fondo, que el usuario no puede usar mientras el scrim este puesto.
      const focusable = focusableIn(panel);
      if (focusable.length === 0) {
        event.preventDefault();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const active = document.activeElement;

      if (event.shiftKey) {
        if (active === first || !panel.contains(active)) {
          event.preventDefault();
          last.focus();
        }
      } else if (active === last || !panel.contains(active)) {
        event.preventDefault();
        first.focus();
      }
    });

    // Atras/adelante mueven la URL sin recargar la pagina: hay que poner el
    // panel donde diga esa URL (y cerrarlo si ya no trae robot).
    window.addEventListener("popstate", () => applyUrlLink());

    // Primera carga. El HTML ya nace en estado "cargando"; esto solo lo hace
    // explicito y arranca la peticion. Si la URL trae `?robot=…`, el panel se
    // abre en cuanto `loadReview` termina de pintar la tabla.
    showView("loading");
    void loadReview();
