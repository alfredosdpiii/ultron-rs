use stitch::*;
use tui::{FullscreenViewport, ScrollView, StdinBuffer};

object!(AnsiDoc, String);
object!(StdinBuf, StdinBuffer);
object!(ScrollObj, ScrollView);
object!(ViewportObj, FullscreenViewport);

datatype!(AnsiText);
datatype!(MdText);
datatype!(MouseSeq);
