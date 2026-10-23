package tea

import "fmt"

// ImageLayer describes an out-of-band terminal image. Commands are emitted
// after text updates in the same synchronized frame. Upload is evaluated only
// on a cache miss; ordinary text updates leave the image untouched.
type ImageLayer struct {
	ID                  uint32
	Revision            uint64
	Upload              fmt.Stringer
	Place, Hide, Forget string
}

func imageEquals(a, b *ImageLayer) bool {
	if a == nil || b == nil {
		return a == b
	}
	return a.ID == b.ID && a.Revision == b.Revision && a.Place == b.Place
}

func (s *cursedRenderer) imageCommands(view *View, cleared bool) string {
	var previous *ImageLayer
	if s.lastView != nil {
		previous = s.lastView.Image
	}
	if cleared {
		clear(s.images)
		s.imageOrder = nil
	}
	next := view.Image
	if !cleared && imageEquals(previous, next) {
		return ""
	}
	commands := ""
	if !cleared && previous != nil && (next == nil || previous.ID != next.ID) {
		commands += previous.Hide
	}
	if next == nil {
		return commands
	}
	if s.images == nil {
		s.images = make(map[uint32]string)
	}
	if _, ok := s.images[next.ID]; !ok {
		if len(s.imageOrder) >= 32 {
			id := s.imageOrder[0]
			commands += s.images[id]
			delete(s.images, id)
			s.imageOrder = s.imageOrder[1:]
		}
		if next.Upload != nil {
			commands += next.Upload.String()
		}
		s.images[next.ID] = next.Forget
	} else {
		commands += next.Place
	}
	for i, id := range s.imageOrder {
		if id == next.ID {
			s.imageOrder = append(s.imageOrder[:i], s.imageOrder[i+1:]...)
			break
		}
	}
	s.imageOrder = append(s.imageOrder, next.ID)
	return commands
}
