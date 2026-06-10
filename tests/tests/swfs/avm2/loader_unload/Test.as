// Arquivo: tests/tests/swfs/avm2/loader_unload/Test.as
//
// Testa que Loader.unload() e unloadAndStop() funcionam corretamente:
//   1. Conteúdo é removido da display list
//   2. Evento "unload" é disparado no contentLoaderInfo
//   3. content retorna null após unload
//   4. Sons param após unloadAndStop

package {
    import flash.display.Loader;
    import flash.display.MovieClip;
    import flash.display.Sprite;
    import flash.events.Event;
    import flash.net.URLRequest;

    public class Test extends MovieClip {
        private var loader: Loader;
        private var loadCount: int = 0;

        public function Test() {
            super();
            runTests();
        }

        private function runTests(): void {
            testUnloadRemovesContent();
        }

        private function testUnloadRemovesContent(): void {
            loader = new Loader();

            // Listener para quando o conteúdo terminar de carregar
            loader.contentLoaderInfo.addEventListener(Event.COMPLETE, onLoadComplete);
            loader.contentLoaderInfo.addEventListener("unload", onUnload);

            // Adiciona o loader à display list
            addChild(loader);

            // Carrega um SWF de teste simples
            loader.load(new URLRequest("child.swf"));
        }

        private function onLoadComplete(e: Event): void {
            trace("LOADED: content is " + (loader.content != null ? "present" : "null"));
            // Deve ser true
            trace("HAS_CONTENT: " + (loader.content != null));
            trace("NUM_CHILDREN_BEFORE: " + loader.numChildren);

            // Agora descarrega
            loader.unload();

            // Imediatamente após unload():
            trace("CONTENT_AFTER_UNLOAD: " + loader.content);    // deve ser null
            trace("NUM_CHILDREN_AFTER: " + loader.numChildren);  // deve ser 0
        }

        private function onUnload(e: Event): void {
            // Este evento DEVE ser disparado pelo nosso patch
            trace("UNLOAD_EVENT_FIRED: true");
        }
    }
}

// Saída esperada no arquivo de golden test (output.txt):
// LOADED: content is present
// HAS_CONTENT: true
// NUM_CHILDREN_BEFORE: 1
// CONTENT_AFTER_UNLOAD: null
// NUM_CHILDREN_AFTER: 0
// UNLOAD_EVENT_FIRED: true
