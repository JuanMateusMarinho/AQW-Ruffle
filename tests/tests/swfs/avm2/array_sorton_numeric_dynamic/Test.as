package {
    import flash.display.Sprite;

    [SWF(width="100", height="100", frameRate="24")]
    public final class Test extends Sprite {
        public function Test() {
            var values:Array = freshValues();
            values.sortOn("oy", Array.NUMERIC);
            trace(ids(values));

            values = freshValues();
            values.sortOn("oy", Array.NUMERIC | Array.DESCENDING);
            trace(ids(values));

            values = freshValues();
            trace(values.sortOn("oy", Array.NUMERIC | Array.RETURNINDEXEDARRAY).join(","));

            values = [{id: "a", oy: 1}, {id: "b", oy: 1}];
            trace(values.sortOn("oy", Array.NUMERIC | Array.UNIQUESORT));
            trace(ids(values));
        }

        private function freshValues():Array {
            return [
                {id: "a", oy: 3},
                {id: "b", oy: -1},
                {id: "c", oy: 2}
            ];
        }

        private function ids(values:Array):String {
            var result:Array = [];
            for each (var value:Object in values) {
                result.push(value.id);
            }
            return result.join(",");
        }
    }
}
