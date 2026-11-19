//! Tipos del informe de errores: lo que el render pinta y la fachada devuelve.
//!
//! Viven aparte del cableado para que `render` y `mcp` los usen sin arrastrar la
//! carga de snapshots ni la atribucion temporal; el como se llenan esta en
//! [`crate::analysis::errors_for`].

/// Longitud a la que se recorta el mensaje de un error.
///
/// Con 160 caracteres caben el defecto y su detalle; la traza de Python de 20
/// lineas no es parte de el y el presupuesto del pack es de ~2,5k tokens.
pub const ERROR_MESSAGE_MAX: usize = 160;

/// Un error del log agrupado por firma.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorItem {
    /// Etiqueta de linea de Studio **en la version donde ocurrio**; vacia si el
    /// nodo no existe en ese snapshot (no se inventa una linea).
    pub line: String,
    /// Comando del nodo que estaba ejecutando el runner.
    pub command: String,
    /// Primera linea del mensaje, acotada a [`ERROR_MESSAGE_MAX`].
    pub message: String,
    /// Veces que aparecio esta firma.
    pub occurrences: usize,
    /// Firma estable (`signature::normalize`): es la clave de agrupacion.
    pub signature: String,
    /// `bots.id` de la version viva en el instante del error, si se pudo atribuir.
    pub version: Option<i64>,
}

/// Errores del robot repartidos por lo que se puede hacer con ellos.
///
/// Sumar los cinco cubos es el panel en rojo permanente que hace que se deje de
/// mirar: `live` es lo accionable hoy; `historical`, evidencia de un defecto ya
/// corregido (o de codigo retirado); `platform` y `noise` no se arreglan desde el
/// robot.
///
/// `PartialEq` fija en un test que reutilizar los logs parseados no cambia un cubo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorReport {
    /// Viven en la version actual y el nodo conserva sus parametros.
    pub live: Vec<ErrorItem>,
    /// Viven en la version actual, pero los parametros del nodo cambiaron.
    pub payload_changed: Vec<ErrorItem>,
    /// Ocurrieron en una version anterior, en un nodo retirado o no se pudieron
    /// atribuir. Es lo que hay que separar del resto: el robot de hoy ya no los
    /// reproduce.
    pub historical: Vec<ErrorItem>,
    /// Defecto de un modulo de la plataforma (`datatransform`, `getbot`...), no del
    /// robot.
    pub platform: Vec<ErrorItem>,
    /// Ruido del motor (`zdev`, `getbot`): fallos del propio Rocketbot.
    pub noise: Vec<ErrorItem>,
}

impl ErrorReport {
    /// Sin nada que contar: el render omite la seccion entera.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Firmas distintas en el informe, no ocurrencias.
    ///
    /// Es lo que muestra la cabecera del render: tres `zdev` iguales son un
    /// problema, no tres, y contarlos como tres infla el panel.
    pub fn total(&self) -> usize {
        self.live.len()
            + self.payload_changed.len()
            + self.historical.len()
            + self.platform.len()
            + self.noise.len()
    }
}
