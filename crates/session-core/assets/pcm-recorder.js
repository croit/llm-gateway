"use strict";(()=>{var e=class extends AudioWorkletProcessor{process(r){let o=r[0]&&r[0][0];return o&&o.length&&this.port.postMessage(o.slice()),!0}};registerProcessor("pcm-recorder",e);})();
